//! The attach-port client: newline JSON-RPC over TCP (DESIGN §4.3.1). It authenticates, calls
//! `zephyr.hello` and `zephyr.subscribe`, then streams events. When the daemon goes away (a
//! re-exec, a restart) it reconnects with backoff and subscribes afresh.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const MIN_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(5);

/// The JSON-RPC code the daemon answers a login call with when it no longer has that login:
/// dropped by its idle deadline, or never started because the eight-pending cap was full. A
/// refusal, not a silence, so the panel can end on it instead of waiting for an answer.
const UNKNOWN_LOGIN: i64 = -32602;

/// `zephyr.hello`'s result and every `zephyr_status` event: the daemon's own types, since the
/// TUI always runs from the daemon's binary.
pub use crate::router::{AccountState, Snapshot};

/// One entry of `zephyr.models`: an account (unnamed without a pool) and what it can run.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AccountModels {
    pub name: Option<String>,
    pub model: Option<String>,
    pub models: Vec<String>,
}

/// One entry of `zephyr.login_providers`: a provider this daemon can sign in to.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Provider {
    /// What `zephyr.login_start` takes, e.g. `claude-code`.
    pub id: String,
    pub label: String,
    /// `oauth` | `key` | `setup_token`
    pub kind: String,
}

impl Provider {
    /// What signing in asks of you, in the panel's own words.
    pub fn how(&self) -> &str {
        match self.kind.as_str() {
            "oauth" => "browser sign-in",
            "key" => "paste an API key",
            "setup_token" => "paste a setup-token code",
            other => other,
        }
    }
}

/// `zephyr.login_start`'s answer: the sign-in the daemon now runs.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Login {
    #[serde(rename = "loginId")]
    pub id: String,
    pub provider: String,
    pub account: Option<String>,
    pub kind: String,
}

/// One `zephyr.login_poll` answer. `waiting` and `need_input` mean it is still running.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LoginStatus {
    /// `waiting` | `need_input` | `done` | `failed`
    pub phase: String,
    /// Where to sign in, when the provider printed one: plain text, since the TUI has no
    /// hyperlinks and no clipboard to open it with.
    pub url: Option<String>,
    /// The tail of the login process's output, escapes already stripped.
    pub lines: Vec<String>,
    /// The account and model the daemon recorded once the sign-in finished.
    pub account: Option<String>,
    pub model: Option<String>,
    pub error: Option<String>,
}

/// What the UI asks of the daemon.
pub enum Ask {
    /// `send {type: "prompt"}`: a message or a slash command.
    Prompt(String),
    /// `zephyr.models`, answered with [`Msg::Models`].
    Models,
    /// `zephyr.login_providers`, answered with [`Msg::Providers`].
    LoginProviders,
    /// `zephyr.login_start`, answered with [`Msg::LoginStarted`]. The TUI always signs a new
    /// account in, so `account` and `model` go out as null; the fields mirror the daemon's
    /// signature so a caller with a name to give can pass one.
    LoginStart {
        provider: String,
        account: Option<String>,
        model: Option<String>,
    },
    /// `zephyr.login_poll`, answered with [`Msg::Login`]. Idempotent, so the frame clock may
    /// send it as often as it likes; [`session`] keeps one in flight per login.
    LoginPoll { login_id: String },
    /// `zephyr.login_input`: one line, the token, to the login process.
    LoginInput { login_id: String, text: String },
    /// `zephyr.login_cancel`.
    LoginCancel { login_id: String },
}

impl std::fmt::Debug for Ask {
    /// The typed token is never printed, not even by `{:?}`: only its length is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ask::Prompt(text) => f.debug_tuple("Prompt").field(text).finish(),
            Ask::Models => f.write_str("Models"),
            Ask::LoginProviders => f.write_str("LoginProviders"),
            Ask::LoginStart {
                provider,
                account,
                model,
            } => f
                .debug_struct("LoginStart")
                .field("provider", provider)
                .field("account", account)
                .field("model", model)
                .finish(),
            Ask::LoginPoll { login_id } => f
                .debug_struct("LoginPoll")
                .field("login_id", login_id)
                .finish(),
            Ask::LoginInput { login_id, text } => f
                .debug_struct("LoginInput")
                .field("login_id", login_id)
                .field("text", &text.chars().count())
                .finish(),
            Ask::LoginCancel { login_id } => f
                .debug_struct("LoginCancel")
                .field("login_id", login_id)
                .finish(),
        }
    }
}

/// What the client tells the UI.
#[derive(Debug)]
pub enum Msg {
    /// Connected and subscribed.
    Up {
        snapshot: Box<Snapshot>,
        messages: Vec<Value>,
    },
    /// One live event (`params.event`).
    Event(Value),
    /// The `zephyr.models` answer; empty from a daemon without it.
    Models(Vec<AccountModels>),
    /// The `zephyr.login_providers` answer; empty from a daemon without it.
    Providers(Vec<Provider>),
    /// `zephyr.login_start`'s answer.
    LoginStarted(Box<Login>),
    /// One `zephyr.login_poll` answer, with the login it was asked about: a late answer for
    /// a login the panel has given up on must not be read as news of whatever runs now.
    Login(String, Box<LoginStatus>),
    /// A login call the daemon refused: an unknown provider, or a login it no longer has.
    LoginError(String),
    /// A login call the daemon answered `-32602` (`unknown login`) about: the login is gone, so
    /// there is nothing left to poll, type into or cancel. Carries the login it was about, so a
    /// late answer for a login the panel no longer runs says nothing about the one it does.
    LoginGone(String),
    /// The connection is gone or was refused; it retries.
    Down(String),
    /// Retrying would not help: a wrong token, or a daemon without `zephyr.hello`.
    Fatal(String),
}

enum Fail {
    Down(String),
    Fatal(String),
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        Fail::Down(match e.kind() {
            ConnectionRefused => "zephyr is not answering yet".into(),
            ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof => {
                "the connection dropped".into()
            }
            _ => e.to_string(),
        })
    }
}

/// Runs until `asks` closes. Each prompt goes out as `send {type: "prompt"}`.
pub async fn run(
    addr: String,
    token: String,
    out: mpsc::UnboundedSender<Msg>,
    mut prompts: mpsc::UnboundedReceiver<Ask>,
) {
    let mut delay = MIN_DELAY;
    // A prompt whose write failed goes out first on the next connection.
    let mut carry = None;
    loop {
        let why = match session(&addr, &token, &out, &mut prompts, &mut carry, &mut delay).await {
            Ok(()) => return,
            Err(Fail::Fatal(e)) => {
                let _ = out.send(Msg::Fatal(e));
                return;
            }
            Err(Fail::Down(e)) => e,
        };
        if out.send(Msg::Down(why)).is_err() {
            return;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_DELAY);
    }
}

async fn session(
    addr: &str,
    token: &str,
    out: &mpsc::UnboundedSender<Msg>,
    prompts: &mut mpsc::UnboundedReceiver<Ask>,
    carry: &mut Option<String>,
    delay: &mut Duration,
) -> Result<(), Fail> {
    let (read, mut write) = TcpStream::connect(addr).await?.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut rpc = Rpc {
        write: &mut write,
        lines: &mut lines,
        id: 0,
    };
    rpc.call("authenticate", json!({"token": token}))
        .await
        .map_err(|e| fatal(e, "the attach token was refused"))?;
    let hello = rpc.call("zephyr.hello", json!({})).await.map_err(|e| {
        fatal(
            e,
            "this zephyr daemon has no zephyr.hello; upgrade it, or use `zephyr attach --stock`",
        )
    })?;
    let sub = rpc
        .call("zephyr.subscribe", json!({}))
        .await
        .map_err(|e| fatal(e, "zephyr.subscribe failed"))?;
    let snapshot = Box::new(serde_json::from_value(hello).unwrap_or_default());
    let messages = match sub {
        Value::Object(mut m) => match m.remove("messages") {
            Some(Value::Array(v)) => v,
            _ => vec![],
        },
        _ => vec![],
    };
    *delay = MIN_DELAY;
    if out.send(Msg::Up { snapshot, messages }).is_err() {
        return Ok(());
    }
    if let Some(text) = carry.take() {
        rpc.prompt(&text, carry).await?;
    }
    // The requests whose answers the UI is waiting for, by JSON-RPC id: an event can arrive
    // before an answer, so answers are matched by id and never read in order. A lost
    // connection drops them all, which is what the panels show as "listing …".
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    // The logins whose poll has not been answered yet. The frame clock asks every 500 ms, so
    // without this a slow daemon would collect one pending poll per login per tick.
    let mut polling: HashSet<String> = HashSet::new();
    loop {
        tokio::select! {
            line = rpc.lines.next_line() => {
                let Some(line) = line? else {
                    return Err(Fail::Down("zephyr closed the connection".into()));
                };
                let Ok(mut v) = serde_json::from_str::<Value>(&line) else { continue };
                let msg = if v["method"] == "event" {
                    Msg::Event(v["params"]["event"].take())
                } else {
                    let Some(what) = v["id"].as_u64().and_then(|id| pending.remove(&id)) else {
                        continue;
                    };
                    // Answered, so the next tick may poll that login again.
                    if let Pending::Poll { login_id } = &what {
                        polling.remove(login_id);
                    }
                    let Some(msg) = answer(what, v) else { continue };
                    msg
                };
                if out.send(msg).is_err() {
                    return Ok(());
                }
            }
            ask = prompts.recv() => match ask {
                Some(Ask::Prompt(text)) => rpc.prompt(&text, carry).await?,
                Some(Ask::Models) => {
                    let id = rpc.request("zephyr.models", json!({})).await?;
                    pending.insert(id, Pending::Models);
                }
                Some(Ask::LoginProviders) => {
                    let id = rpc.request("zephyr.login_providers", json!({})).await?;
                    pending.insert(id, Pending::Providers);
                }
                Some(Ask::LoginStart { provider, account, model }) => {
                    let params = json!({"provider": provider, "account": account, "model": model});
                    let id = rpc.request("zephyr.login_start", params).await?;
                    pending.insert(id, Pending::Start);
                }
                Some(Ask::LoginPoll { login_id }) => {
                    // One poll per login at a time: a skipped tick costs nothing, since the
                    // answer still says where the sign-in is.
                    if polling.insert(login_id.clone()) {
                        let params = json!({"loginId": login_id});
                        let id = rpc.request("zephyr.login_poll", params).await?;
                        pending.insert(id, Pending::Poll { login_id });
                    }
                }
                Some(Ask::LoginInput { login_id, text }) => {
                    let params = json!({"loginId": login_id, "text": text});
                    let id = rpc.request("zephyr.login_input", params).await?;
                    // Not quiet: the panel has already taken the typed token, so a refusal —
                    // cancelled from `zephyr chat`, child gone, registry evicted — has to
                    // reach the UI, or the prompt waits for a code that can never arrive.
                    pending.insert(id, Pending::Input { login_id });
                }
                Some(Ask::LoginCancel { login_id }) => {
                    // The UI has given up on this sign-in, so a poll of it that never came
                    // back must not keep the next one from going out. The call is quiet either
                    // way: `-32602` here says the login is already gone, which is what the
                    // cancel asked for.
                    polling.remove(&login_id);
                    let id = rpc.request("zephyr.login_cancel", json!({"loginId": login_id})).await?;
                    pending.insert(id, Pending::Quiet);
                }
                None => return Ok(()),
            },
        }
    }
}

/// A request whose answer the UI is waiting for.
enum Pending {
    Models,
    Providers,
    Start,
    /// A poll, with the login it asks about so the coalescing entry can be pruned.
    Poll {
        login_id: String,
    },
    /// `login_input` answers `{}` when it took the token, an error when the login is gone.
    Input {
        login_id: String,
    },
    /// `login_cancel` answers `{}`: nothing to show either way.
    Quiet,
}

/// An error answer to one of the login calls.
struct Refusal {
    why: String,
    /// `UNKNOWN_LOGIN`: the daemon no longer has the login, so the panel ends without a word
    /// rather than showing a failure that is not the user's to read.
    gone: bool,
}

/// The message an answer becomes, or `None` for one the UI does not show.
fn answer(what: Pending, mut v: Value) -> Option<Msg> {
    let refusal = v.get("error").map(|e| Refusal {
        why: e["message"]
            .as_str()
            .unwrap_or("the sign-in failed")
            .to_string(),
        gone: e["code"].as_i64() == Some(UNKNOWN_LOGIN),
    });
    let mut result = v["result"].take();
    Some(match what {
        // A daemon without these answers an error, so the panel shows an empty list rather
        // than the UI closing on a method it never had.
        Pending::Models => {
            Msg::Models(serde_json::from_value(result["accounts"].take()).unwrap_or_default())
        }
        Pending::Providers => {
            Msg::Providers(serde_json::from_value(result["providers"].take()).unwrap_or_default())
        }
        // A start is refused for a provider this daemon cannot sign in to, which the panel has
        // to say: the user picked it from the list, so silence would lose the Enter.
        Pending::Start => match refusal {
            Some(r) => Msg::LoginError(r.why),
            None => Msg::LoginStarted(Box::new(serde_json::from_value(result).unwrap_or_default())),
        },
        // A poll the daemon refused carries the login id too: a refusal for a login the panel
        // no longer runs says nothing about this one. `unknown login` ends the panel quietly —
        // there is no sign-in left to show — while any other refusal is that login's last
        // word and is drawn as the failure it is.
        Pending::Poll { login_id } => match refusal {
            Some(r) if r.gone => Msg::LoginGone(login_id),
            Some(r) => Msg::Login(
                login_id,
                Box::new(LoginStatus {
                    phase: "failed".into(),
                    error: Some(r.why),
                    ..Default::default()
                }),
            ),
            None => {
                let mut status: LoginStatus = serde_json::from_value(result).unwrap_or_default();
                // The login process's own colours would fight the panel's, and its cursor
                // moves would land in the middle of a drawn row.
                for line in &mut status.lines {
                    *line = rpi_tui::strip_ansi(line);
                }
                Msg::Login(login_id, Box::new(status))
            }
        },
        // The token went to a login the daemon had already dropped: the user typed into
        // nothing, so the panel ends quietly instead of reporting a failure.
        Pending::Input { login_id } => match refusal {
            Some(r) if r.gone => Msg::LoginGone(login_id),
            Some(r) => Msg::LoginError(r.why),
            None => return None,
        },
        Pending::Quiet => return None,
    })
}

/// A JSON-RPC error answer becomes fatal with `what`; a lost connection stays retryable.
fn fatal(e: Fail, what: &str) -> Fail {
    match e {
        Fail::Fatal(detail) => Fail::Fatal(format!("{what} ({detail})")),
        down => down,
    }
}

struct Rpc<'a> {
    write: &'a mut OwnedWriteHalf,
    lines: &'a mut Lines<BufReader<OwnedReadHalf>>,
    id: u64,
}

impl Rpc<'_> {
    async fn request(&mut self, method: &str, params: Value) -> std::io::Result<u64> {
        self.id += 1;
        let req = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params});
        self.write.write_all(format!("{req}\n").as_bytes()).await?;
        Ok(self.id)
    }

    /// Sends a request and waits for its answer; nothing else arrives before subscribe.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Fail> {
        let id = self.request(method, params).await?;
        loop {
            let Some(line) = self.lines.next_line().await? else {
                return Err(Fail::Down("zephyr closed the connection".into()));
            };
            let Ok(mut v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v["id"] != id {
                continue;
            }
            if let Some(e) = v.get("error") {
                return Err(Fail::Fatal(
                    e["message"].as_str().unwrap_or("error").to_string(),
                ));
            }
            return Ok(v["result"].take());
        }
    }

    async fn prompt(&mut self, text: &str, carry: &mut Option<String>) -> Result<(), Fail> {
        let params = json!({"sessionId": "main", "command": {"type": "prompt", "content": text}});
        if let Err(e) = self.request("send", params).await {
            *carry = Some(text.to_string());
            return Err(e.into());
        }
        Ok(())
    }
}

/// The tests' scripted attach daemon: answers the three handshake calls (and refuses a
/// wrong token), so a test can then script whatever the panel asks for. Shared with
/// `tui::mod`'s tests, which drive the same fake to a login panel.
#[cfg(test)]
pub(crate) async fn handshake(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
    write: &mut OwnedWriteHalf,
    token: &str,
    model: &str,
) -> Option<()> {
    for _ in 0..3 {
        let req: Value = serde_json::from_str(&lines.next_line().await.ok()??).ok()?;
        let id = &req["id"];
        let answer = match req["method"].as_str()? {
            "authenticate" if req["params"]["token"] == token => {
                json!({"jsonrpc": "2.0", "id": id, "result": {"authenticated": true}})
            }
            "authenticate" => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "invalid token"}})
            }
            "zephyr.hello" => {
                json!({"jsonrpc": "2.0", "id": id, "result": {"session": "main", "model": model, "run": {"busy": false}}})
            }
            "zephyr.subscribe" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "subscriptionId": 1, "sessionId": "main", "status": "subscribed",
                "messages": [{"kind": "user", "role": "user", "content": "[discord #1] hi", "text": "hi", "origin": {"channel": "discord", "id": 1}}]}}),
            _ => return None,
        };
        write
            .write_all(format!("{answer}\n").as_bytes())
            .await
            .ok()?;
        if answer.get("error").is_some() {
            return None;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn reconnects_and_resubscribes_after_the_daemon_goes_away() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            // First daemon: handshake, one event, then gone (a re-exec).
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/one").await.unwrap();
            let ev = json!({"jsonrpc": "2.0", "method": "event", "params": {"subscriptionId": 1, "event": {"type": "ready"}}});
            w.write_all(format!("{ev}\n").as_bytes()).await.unwrap();
            drop((lines, w));
            // Second daemon: the prompt typed meanwhile arrives after the fresh handshake.
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/two").await.unwrap();
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            (req, w)
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));

        let Some(Msg::Up { snapshot, messages }) = rx.recv().await else {
            panic!("no first Up")
        };
        assert_eq!(snapshot.model.as_deref(), Some("a/one"));
        assert_eq!(messages[0]["text"], "hi");
        assert!(matches!(rx.recv().await, Some(Msg::Event(e)) if e["type"] == "ready"));
        assert!(matches!(rx.recv().await, Some(Msg::Down(_))));
        ptx.send(Ask::Prompt("typed while away".into())).unwrap();
        let Some(Msg::Up { snapshot, .. }) = rx.recv().await else {
            panic!("no second Up")
        };
        assert_eq!(snapshot.model.as_deref(), Some("a/two"));
        let (req, _w) = server.await.unwrap();
        assert_eq!(req["method"], "send");
        assert_eq!(req["params"]["command"]["type"], "prompt");
        assert_eq!(req["params"]["command"]["content"], "typed while away");
    }

    #[tokio::test]
    async fn models_are_asked_for_and_answered() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/one").await.unwrap();
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(req["method"], "zephyr.models");
            // An event first: the answer is matched by id, not by order.
            let ev = json!({"jsonrpc": "2.0", "method": "event", "params": {"subscriptionId": 1, "event": {"type": "ready"}}});
            let answer = json!({"jsonrpc": "2.0", "id": req["id"], "result": {"accounts": [
                {"name": "work", "model": "a/one", "models": ["a/one", "a/two"]}]}});
            w.write_all(format!("{ev}\n{answer}\n").as_bytes())
                .await
                .unwrap();
            lines.next_line().await.ok();
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(Msg::Up { .. })));
        ptx.send(Ask::Models).unwrap();
        assert!(matches!(rx.recv().await, Some(Msg::Event(e)) if e["type"] == "ready"));
        let Some(Msg::Models(list)) = rx.recv().await else {
            panic!("no models")
        };
        assert_eq!(list[0].name.as_deref(), Some("work"));
        assert_eq!(list[0].models, ["a/one", "a/two"]);
    }

    #[tokio::test]
    async fn logins_are_asked_for_and_answered_by_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/one").await.unwrap();
            let mut asked = vec![];
            let reply = |req: Value, result: Value| json!({"jsonrpc": "2.0", "id": req["id"], "result": result});
            for result in [
                json!({"providers": [
                    {"id": "claude-code", "label": "Claude (Agent SDK)", "kind": "setup_token"},
                    {"id": "opencode-go", "label": "OpenCode Go", "kind": "key"}]}),
                json!({"loginId": "l1", "provider": "claude-code", "account": null, "kind": "setup_token"}),
                // The login process's own escapes are stripped on the way to the UI.
                json!({"phase": "need_input", "url": "https://claude.ai/oauth", "lines": ["\u{1b}[32mvisit\u{1b}[0m it"],
                       "account": null, "model": null, "error": null}),
                json!({}),
                json!({}),
            ] {
                let req: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let answer = reply(req.clone(), result);
                asked.push(req);
                w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            }
            asked
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(Msg::Up { .. })));

        ptx.send(Ask::LoginProviders).unwrap();
        let Some(Msg::Providers(list)) = rx.recv().await else {
            panic!("no providers")
        };
        assert_eq!(list[1].id, "opencode-go");
        assert_eq!(list[0].how(), "paste a setup-token code");

        let start = Ask::LoginStart {
            provider: "claude-code".into(),
            account: None,
            model: None,
        };
        ptx.send(start).unwrap();
        let Some(Msg::LoginStarted(login)) = rx.recv().await else {
            panic!("no login")
        };
        assert_eq!(login.id, "l1");
        assert_eq!(login.kind, "setup_token");

        ptx.send(Ask::LoginPoll {
            login_id: "l1".into(),
        })
        .unwrap();
        let Some(Msg::Login(login_id, status)) = rx.recv().await else {
            panic!("no poll")
        };
        // The answer names the login it was asked about, not just the phase it was in.
        assert_eq!(login_id, "l1");
        assert_eq!(status.phase, "need_input");
        assert_eq!(status.lines, ["visit it"]);
        assert_eq!(status.url.as_deref(), Some("https://claude.ai/oauth"));

        // Input and cancel answer `{}`: no message, so the next one is the event after them.
        ptx.send(Ask::LoginInput {
            login_id: "l1".into(),
            text: "sk-ant".into(),
        })
        .unwrap();
        ptx.send(Ask::LoginCancel {
            login_id: "l1".into(),
        })
        .unwrap();
        let asked = server.await.unwrap();
        let methods: Vec<&str> = asked.iter().filter_map(|r| r["method"].as_str()).collect();
        assert_eq!(
            methods,
            [
                "zephyr.login_providers",
                "zephyr.login_start",
                "zephyr.login_poll",
                "zephyr.login_input",
                "zephyr.login_cancel"
            ]
        );
        assert_eq!(asked[1]["params"]["provider"], "claude-code");
        assert_eq!(asked[1]["params"]["account"], Value::Null);
        assert_eq!(asked[2]["params"]["loginId"], "l1");
        assert_eq!(asked[3]["params"]["text"], "sk-ant");
    }

    #[tokio::test]
    async fn a_token_the_daemon_no_longer_wants_says_the_login_is_gone() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/one").await.unwrap();
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            // The daemon evicted the login; the panel already forgot what was typed.
            let answer = json!({"jsonrpc": "2.0", "id": req["id"],
                                "error": {"code": -32602, "message": "unknown login"}});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            lines.next_line().await.ok();
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(Msg::Up { .. })));
        ptx.send(Ask::LoginInput {
            login_id: "gone".into(),
            text: "sk-ant".into(),
        })
        .unwrap();
        // The token went to a login that no longer exists: not an error to read, but the end of
        // the panel, which still names the login so a late one cannot end the wrong sign-in.
        assert!(matches!(rx.recv().await, Some(Msg::LoginGone(id)) if id == "gone"));
    }

    #[tokio::test]
    async fn a_refused_poll_names_the_login_it_was_about() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/one").await.unwrap();
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            // Not `unknown login`: a daemon that is there and says the sign-in failed.
            let answer = json!({"jsonrpc": "2.0", "id": req["id"],
                                "error": {"code": -32000, "message": "the child died"}});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            lines.next_line().await.ok();
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(Msg::Up { .. })));
        ptx.send(Ask::LoginPoll {
            login_id: "gone".into(),
        })
        .unwrap();
        // A refusal is that login's last word: it carries the id, so the panel can tell
        // whether the login it is showing is the one that failed.
        let Some(Msg::Login(login_id, status)) = rx.recv().await else {
            panic!("no poll answer")
        };
        assert_eq!(login_id, "gone");
        assert_eq!(status.phase, "failed");
        assert_eq!(status.error.as_deref(), Some("the child died"));
    }

    #[tokio::test]
    async fn a_poll_of_a_login_the_daemon_dropped_says_it_is_gone() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/one").await.unwrap();
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            // The idle deadline dropped the login while the panel was still polling it.
            let answer = json!({"jsonrpc": "2.0", "id": req["id"],
                                "error": {"code": -32602, "message": "unknown login"}});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            lines.next_line().await.ok();
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(Msg::Up { .. })));
        ptx.send(Ask::LoginPoll {
            login_id: "gone".into(),
        })
        .unwrap();
        // A poll that timed out says `failed`; a login the daemon no longer has is not a
        // failure, it is the panel's end.
        assert!(matches!(rx.recv().await, Some(Msg::LoginGone(id)) if id == "gone"));
    }

    #[tokio::test]
    async fn a_cancel_frees_the_poll_it_gave_up_on() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/one").await.unwrap();
            let mut asked = vec![];
            // The first poll is never answered, as a lost frame would be: it must not keep
            // the login from being polled again once the panel has given up on it.
            for _ in 0..3 {
                let req: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                asked.push(req);
            }
            asked
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(Msg::Up { .. })));
        ptx.send(Ask::LoginPoll {
            login_id: "l1".into(),
        })
        .unwrap();
        // Held back long enough that the ask queue is drained in order.
        tokio::time::sleep(Duration::from_millis(150)).await;
        ptx.send(Ask::LoginCancel {
            login_id: "l1".into(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        ptx.send(Ask::LoginPoll {
            login_id: "l1".into(),
        })
        .unwrap();
        let asked = server.await.unwrap();
        let methods: Vec<&str> = asked.iter().filter_map(|r| r["method"].as_str()).collect();
        assert_eq!(
            methods,
            [
                "zephyr.login_poll",
                "zephyr.login_cancel",
                "zephyr.login_poll"
            ]
        );
    }

    #[tokio::test]
    async fn only_one_poll_per_login_is_outstanding() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", "a/one").await.unwrap();
            let first: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(first["method"], "zephyr.login_poll");
            // The tick that races the answer is skipped, not queued behind it.
            let extra = tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await;
            assert!(extra.is_err(), "a second poll went out: {extra:?}");
            let status = json!({"phase": "waiting", "url": null, "lines": [],
                                "account": null, "model": null, "error": null});
            let answer = json!({"jsonrpc": "2.0", "id": first["id"], "result": status});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            // Once the answer is in, the entry is pruned and the next tick asks again.
            let second: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(second["method"], "zephyr.login_poll");
            let answer = json!({"jsonrpc": "2.0", "id": second["id"], "result": {"phase": "done"}});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            (first, second)
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(Msg::Up { .. })));
        for _ in 0..2 {
            ptx.send(Ask::LoginPoll {
                login_id: "l1".into(),
            })
            .unwrap();
        }
        // The first answer arrives; the second ask was coalesced, so nothing else came.
        let Some(Msg::Login(login_id, status)) = rx.recv().await else {
            panic!("no poll answer")
        };
        assert_eq!(login_id, "l1");
        assert_eq!(status.phase, "waiting");
        ptx.send(Ask::LoginPoll {
            login_id: "l1".into(),
        })
        .unwrap();
        let Some(Msg::Login(_, status)) = rx.recv().await else {
            panic!("no second poll answer")
        };
        assert_eq!(status.phase, "done");
        // Both were polls of the same login, one id apart in the daemon's eyes.
        let (first, second) = server.await.unwrap();
        assert_eq!(first["params"]["loginId"], "l1");
        assert_eq!(second["params"]["loginId"], "l1");
    }

    #[test]
    fn a_typed_token_is_counted_not_printed() {
        let ask = Ask::LoginInput {
            login_id: "l1".into(),
            text: "sk-ant-oat".into(),
        };
        let shown = format!("{ask:?}");
        assert!(!shown.contains("sk-ant"), "{shown}");
        assert!(shown.contains("text: 10"), "{shown}");
    }

    #[tokio::test]
    async fn a_refused_token_is_fatal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "right", "m").await;
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ptx, prx) = mpsc::unbounded_channel();
        run(addr, "wrong".into(), tx, prx).await;
        assert!(matches!(rx.recv().await, Some(Msg::Fatal(e)) if e.contains("token was refused")));
    }
}
