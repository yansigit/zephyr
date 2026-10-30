//! The attach-port client: newline JSON-RPC over TCP (DESIGN §4.3.1). It authenticates, calls
//! `zephyr.hello` and `zephyr.subscribe`, then streams events. When the daemon goes away (a
//! re-exec, a restart) it reconnects with backoff and subscribes afresh.

use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const MIN_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(5);

/// `zephyr.hello`'s result and every `zephyr_status` event.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub version: String,
    pub session: String,
    pub account: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub paused: Option<String>,
    pub channels: Vec<ChannelState>,
    pub spend: Spend,
    pub queue: Vec<Queued>,
    pub failed_replies: i64,
    pub run: Run,
    pub accounts: Vec<AccountState>,
    pub jobs: Vec<NextJob>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AccountState {
    pub name: String,
    pub model: String,
    /// `ready`, `cooling` or `needs login`.
    pub state: String,
    pub until_unix: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct NextJob {
    pub id: i64,
    pub name: String,
    pub next_unix: i64,
}

/// One entry of `zephyr.models`: an account (unnamed without a pool) and what it can run.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AccountModels {
    pub name: Option<String>,
    pub model: Option<String>,
    pub models: Vec<String>,
}

/// What the UI asks of the daemon.
#[derive(Debug)]
pub enum Ask {
    /// `send {type: "prompt"}`: a message or a slash command.
    Prompt(String),
    /// `zephyr.models`, answered with [`Msg::Models`].
    Models,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ChannelState {
    pub name: String,
    /// `connected`, `off` or `reconnecting`.
    pub state: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Spend {
    pub tokens: i64,
    pub cost: f64,
    pub daily_tokens_limit: Option<i64>,
    pub daily_cost_limit: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Queued {
    pub channel: String,
    pub preview: String,
    pub since_unix: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Run {
    pub busy: bool,
    pub since_unix_ms: Option<i64>,
    pub origin: Option<Origin>,
    pub current_tool: Option<String>,
    pub tools_done: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Origin {
    pub channel: String,
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
    // The pending `zephyr.models` request; a lost connection drops it.
    let mut models = None;
    loop {
        tokio::select! {
            line = rpc.lines.next_line() => {
                let Some(line) = line? else {
                    return Err(Fail::Down("zephyr closed the connection".into()));
                };
                let Ok(mut v) = serde_json::from_str::<Value>(&line) else { continue };
                let msg = if v["method"] == "event" {
                    Msg::Event(v["params"]["event"].take())
                } else if models.is_some() && v["id"].as_u64() == models {
                    models = None;
                    let list = v["result"]["accounts"].take();
                    Msg::Models(serde_json::from_value(list).unwrap_or_default())
                } else {
                    continue;
                };
                if out.send(msg).is_err() {
                    return Ok(());
                }
            }
            ask = prompts.recv() => match ask {
                Some(Ask::Prompt(text)) => rpc.prompt(&text, carry).await?,
                Some(Ask::Models) => models = Some(rpc.request("zephyr.models", json!({})).await?),
                None => return Ok(()),
            },
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A scripted attach server: answers the handshake, replays `messages`, then runs `then`.
    async fn handshake(
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
