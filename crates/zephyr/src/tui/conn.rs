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
/// A method the daemon does not have: an older zephyr.
const METHOD_NOT_FOUND: i64 = -32601;

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

/// One entry of the vault as `zephyr.vault_*` lists it: never a value (DESIGN §4.15).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct VaultEntry {
    /// `login` | `token`
    pub kind: String,
    pub name: String,
    pub site: String,
    pub username: String,
    /// Whether a one-time code seed is saved.
    pub totp: bool,
    pub granted: bool,
    pub programs: Vec<String>,
    /// The `host:port`s a token is sent to (DESIGN §4.21), and whether it is handed out raw.
    pub hosts: Vec<String>,
    pub raw: bool,
}

impl VaultEntry {
    /// Where a token goes, in a few words.
    pub fn sent(&self) -> String {
        let hosts: Vec<&str> = self.hosts.iter().map(|h| crate::vault::shown(h)).collect();
        match (self.raw, hosts.is_empty()) {
            (true, _) => "handed out as its real value (raw)".into(),
            (false, true) => format!("bound to no host yet: /vault bind {} HOST..", self.name),
            (false, false) => format!("sent only to {}", hosts.join(", ")),
        }
    }

    /// The card shown once the owner saves this entry: what was saved and what it may do, never
    /// a value. `updated`: the name was already in the vault, so the agent was not told.
    pub fn saved(&self, updated: bool) -> String {
        let verb = if updated { "updated" } else { "saved" };
        let mut rows = vec![format!("{verb} the {} {}", self.kind, self.name)];
        if self.kind == "login" {
            rows.push(format!("site  {}", self.site));
            if !self.username.is_empty() {
                rows.push(format!("user  {}", self.username));
            }
            if self.totp {
                rows.push("code  one-time codes from the saved seed".into());
            }
            rows.push(match self.granted {
                true => "fill  allowed".into(),
                false => "fill  the first one waits for your OK".into(),
            });
        } else {
            rows.push(match self.programs.as_slice() {
                [] => "use   each program needs your OK once".into(),
                p => format!("use   allowed for {}", p.join(", ")),
            });
            rows.push(format!("host  {}", self.sent()));
        }
        if !updated {
            rows.push("the agent was told it can use it now".into());
        }
        rows.join("\n")
    }
}

/// One private fact as `zephyr.facts_*` lists it: never its value (approvals spec §4.5).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct FactEntry {
    pub name: String,
    /// `text` | `digits` | `email`
    pub kind: String,
    /// Where it may go without a check.
    pub allow: Vec<String>,
}

/// A saved main session as `zephyr.sessions` lists it, newest first.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SessionInfo {
    pub id: String,
    /// Its `/rename` name.
    pub name: Option<String>,
    /// When it last changed, unix secs.
    pub at: i64,
}

/// An exchange `/undo N` counts, as `zephyr.turns` lists them, newest first.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Turn {
    pub channel: String,
    /// The message, untagged.
    pub text: String,
}

/// What the UI asks of the daemon.
pub enum Ask {
    /// `send {type: "prompt"}`: a message or a slash command.
    Prompt(String),
    /// The same with `followUp: true`: it waits for the run to settle instead of steering in.
    FollowUp(String),
    /// `zephyr.dequeue`, answered with [`Msg::Dequeued`].
    Dequeue,
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
    /// `zephyr.vault_list`; this and the three below are answered with [`Msg::Vault`].
    VaultList,
    /// `zephyr.vault_put`: the entry, values and all, for `vault.toml` only.
    VaultPut(Box<crate::vault::Entry>),
    /// `zephyr.vault_rm`.
    VaultRm(String),
    /// `zephyr.vault_revoke`: every grant of the entry.
    VaultRevoke(String),
    /// `zephyr.sessions`, answered with [`Msg::Sessions`].
    Sessions,
    /// `zephyr.turns`, answered with [`Msg::Turns`].
    Turns,
    /// `zephyr.settings`, answered with [`Msg::Settings`].
    Settings,
    /// `zephyr.settings_set`: a change as the owner on the TUI, a strong channel; `until` is a
    /// time limit like `2h`. Answered with [`Msg::SettingResult`].
    SettingsSet {
        key: String,
        value: String,
        until: Option<String>,
    },
    /// `zephyr.proposals`, answered with [`Msg::Proposals`].
    Proposals,
    /// `zephyr.decide` (`ok`, `once`, `no` or `cancel`), answered with [`Msg::Decided`].
    Decide { id: i64, verb: String },
    /// `zephyr.activity`: the grants, or the record's page before `before` (the first page
    /// without one). Answered with [`Msg::Activity`].
    Activity {
        grants: bool,
        before: Option<String>,
        /// The panel's opening that asks, handed back with the answer.
        generation: u64,
    },
    /// `zephyr.facts_list`; this and the two below are answered with [`Msg::Facts`].
    FactsList,
    /// `zephyr.facts_put`: the fact, value and all, for `facts.toml` only. An empty value keeps
    /// the saved one.
    FactsPut(Box<crate::facts::Fact>),
    /// `zephyr.facts_rm`.
    FactsRm(String),
}

impl std::fmt::Debug for Ask {
    /// The typed token is never printed, not even by `{:?}`: only its length is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ask::Prompt(text) => f.debug_tuple("Prompt").field(text).finish(),
            Ask::FollowUp(text) => f.debug_tuple("FollowUp").field(text).finish(),
            Ask::Dequeue => f.write_str("Dequeue"),
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
            Ask::VaultList => f.write_str("VaultList"),
            // `Entry`'s own Debug prints only the values' lengths.
            Ask::VaultPut(entry) => f.debug_tuple("VaultPut").field(entry).finish(),
            Ask::VaultRm(name) => f.debug_tuple("VaultRm").field(name).finish(),
            Ask::VaultRevoke(name) => f.debug_tuple("VaultRevoke").field(name).finish(),
            Ask::Sessions => f.write_str("Sessions"),
            Ask::Turns => f.write_str("Turns"),
            Ask::Settings => f.write_str("Settings"),
            // No setting holds a secret (settings spec §4.4); private facts get asks of their own.
            Ask::SettingsSet { key, value, until } => f
                .debug_struct("SettingsSet")
                .field("key", key)
                .field("value", value)
                .field("until", until)
                .finish(),
            Ask::Proposals => f.write_str("Proposals"),
            Ask::Decide { id, verb } => f
                .debug_struct("Decide")
                .field("id", id)
                .field("verb", verb)
                .finish(),
            Ask::FactsList => f.write_str("FactsList"),
            // `Fact`'s own Debug prints only the value's length.
            Ask::FactsPut(fact) => f.debug_tuple("FactsPut").field(fact).finish(),
            Ask::FactsRm(name) => f.debug_tuple("FactsRm").field(name).finish(),
            Ask::Activity {
                grants,
                before,
                generation,
            } => f
                .debug_struct("Activity")
                .field("grants", grants)
                .field("before", before)
                .field("generation", generation)
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
    /// The vault's entries, after a `zephyr.vault_*` call.
    Vault(Vec<VaultEntry>),
    /// The messages `zephyr.dequeue` took back, oldest first.
    Dequeued(Vec<String>),
    /// A vault call the daemon refused, and why.
    VaultError(String),
    /// The `zephyr.sessions` answer; empty from a daemon without it.
    Sessions(Vec<SessionInfo>),
    /// The `zephyr.turns` answer; empty from a daemon without it.
    Turns(Vec<Turn>),
    /// `zephyr.settings`' answer, or `{"error": why}`.
    Settings(Value),
    /// `zephyr.settings_set`'s answer: `changed`, `proposed` or `refused` (an error answer is a
    /// refusal with the error as its reason).
    SettingResult(Value),
    /// `zephyr.proposals`' answer; `{"proposals": []}` from a daemon without it.
    Proposals(Value),
    /// `zephyr.decide`'s answer, `{"ok", "text"}`; an error answer is `ok: false` with its text.
    Decided(Value),
    /// `zephyr.activity`'s answer, for the Grants tab or the Activity tab (`append`: a page
    /// asked for with `before`). A daemon without it (`-32601`) answers `{"entries": [],
    /// "next": null, "old": true}`, another error `{"error": why}`.
    Activity {
        grants: bool,
        append: bool,
        generation: u64,
        answer: Value,
    },
    /// The private facts, after a `zephyr.facts_*` call: names, kinds and allowed targets.
    Facts(Vec<FactEntry>),
    /// A facts call the daemon refused, and why (never with a value).
    FactsError(String),
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
    carry: &mut Option<(String, bool)>,
    delay: &mut Duration,
) -> Result<(), Fail> {
    let (read, mut write) = TcpStream::connect(addr).await?.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut rpc = Rpc {
        write: &mut write,
        lines: &mut lines,
        id: 0,
    };
    // The token never crosses the socket (DESIGN §4.3.1): the daemon proves it holds it first,
    // so a program holding the attach port while the daemon is down learns nothing, and the
    // TUI stops there rather than send it the vault's or the facts' values.
    use crate::attach::{proof, proven, BY_CLIENT, BY_DAEMON};
    let impostor = format!(
        "whatever answers on {addr} could not prove it is your zephyr daemon, so nothing was \
         sent to it: another program may hold the attach port (run zephyr restart, then \
         attach again), or this attach token is not the daemon's"
    );
    let (client, hex) = crate::attach::nonce();
    let said = rpc.call("zephyr.challenge", json!({"nonce": hex})).await;
    let said = said.map_err(|e| fatal(e, &impostor))?;
    let daemon = (said["nonce"].as_str())
        .and_then(|n| data_encoding::HEXLOWER.decode(n.as_bytes()).ok())
        .filter(|n| n.len() == crate::attach::NONCE);
    let given = said["proof"].as_str().unwrap_or_default();
    let Some(daemon) = daemon.filter(|d| proven(token, BY_DAEMON, &client, d, given)) else {
        return Err(Fail::Fatal(impostor));
    };
    let mine = proof(token, BY_CLIENT, &client, &daemon);
    rpc.call("zephyr.prove", json!({"proof": mine}))
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
    if let Some((text, after)) = carry.take() {
        rpc.prompt(&text, after, carry).await?;
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
                let Some(mut v) = parse(&line) else { continue };
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
                Some(Ask::Prompt(text)) => rpc.prompt(&text, false, carry).await?,
                Some(Ask::FollowUp(text)) => rpc.prompt(&text, true, carry).await?,
                Some(Ask::Dequeue) => {
                    let id = rpc.request("zephyr.dequeue", json!({})).await?;
                    pending.insert(id, Pending::Dequeue);
                }
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
                Some(Ask::VaultList) => {
                    let id = rpc.request("zephyr.vault_list", json!({})).await?;
                    pending.insert(id, Pending::Vault);
                }
                Some(Ask::VaultPut(entry)) => {
                    let params = serde_json::to_value(&*entry).unwrap_or_default();
                    let id = rpc.request("zephyr.vault_put", params).await?;
                    pending.insert(id, Pending::Vault);
                }
                Some(Ask::VaultRm(name)) => {
                    let id = rpc.request("zephyr.vault_rm", json!({"name": name})).await?;
                    pending.insert(id, Pending::Vault);
                }
                Some(Ask::VaultRevoke(name)) => {
                    let id = rpc.request("zephyr.vault_revoke", json!({"name": name})).await?;
                    pending.insert(id, Pending::Vault);
                }
                Some(Ask::Sessions) => {
                    let id = rpc.request("zephyr.sessions", json!({})).await?;
                    pending.insert(id, Pending::Sessions);
                }
                Some(Ask::Turns) => {
                    let id = rpc.request("zephyr.turns", json!({})).await?;
                    pending.insert(id, Pending::Turns);
                }
                Some(Ask::Settings) => {
                    let id = rpc.request("zephyr.settings", json!({})).await?;
                    pending.insert(id, Pending::Settings);
                }
                Some(Ask::SettingsSet { key, value, until }) => {
                    let params = json!({"key": key, "value": value, "until": until});
                    let id = rpc.request("zephyr.settings_set", params).await?;
                    pending.insert(id, Pending::SettingsSet);
                }
                Some(Ask::Proposals) => {
                    let id = rpc.request("zephyr.proposals", json!({})).await?;
                    pending.insert(id, Pending::Proposals);
                }
                Some(Ask::Decide { id, verb }) => {
                    let params = json!({"id": id, "verb": verb});
                    let id = rpc.request("zephyr.decide", params).await?;
                    pending.insert(id, Pending::Decide);
                }
                Some(Ask::Activity {
                    grants,
                    before,
                    generation,
                }) => {
                    let params = match grants {
                        true => json!({"kind": "grants"}),
                        false => json!({"before": before}),
                    };
                    let id = rpc.request("zephyr.activity", params).await?;
                    let append = before.is_some();
                    pending.insert(
                        id,
                        Pending::Activity {
                            grants,
                            append,
                            generation,
                        },
                    );
                }
                Some(Ask::FactsList) => {
                    let id = rpc.request("zephyr.facts_list", json!({})).await?;
                    pending.insert(id, Pending::Facts);
                }
                Some(Ask::FactsPut(fact)) => {
                    let params = json!({"name": fact.name, "kind": fact.kind.key(),
                                        "value": fact.value, "allow": fact.allow});
                    let id = rpc.request("zephyr.facts_put", params).await?;
                    pending.insert(id, Pending::Facts);
                }
                Some(Ask::FactsRm(name)) => {
                    let id = rpc.request("zephyr.facts_rm", json!({"name": name})).await?;
                    pending.insert(id, Pending::Facts);
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
    /// Any `zephyr.vault_*` call: the entries, or why not.
    Vault,
    Dequeue,
    Sessions,
    Turns,
    Settings,
    SettingsSet,
    Proposals,
    Decide,
    /// Any `zephyr.facts_*` call: the facts, or why not.
    Facts,
    Activity {
        grants: bool,
        append: bool,
        generation: u64,
    },
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
    // The settings calls show the daemon's own words for a refusal.
    let error = (v.get("error")).map(|e| e["message"].as_str().unwrap_or("refused").to_string());
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
        // A daemon without it answers an error: nothing was taken back.
        Pending::Dequeue => {
            Msg::Dequeued(serde_json::from_value(result["texts"].take()).unwrap_or_default())
        }
        Pending::Vault => match refusal {
            Some(r) => Msg::VaultError(r.why),
            None => {
                Msg::Vault(serde_json::from_value(result["entries"].take()).unwrap_or_default())
            }
        },
        Pending::Sessions => {
            Msg::Sessions(serde_json::from_value(result["sessions"].take()).unwrap_or_default())
        }
        Pending::Turns => {
            Msg::Turns(serde_json::from_value(result["turns"].take()).unwrap_or_default())
        }
        Pending::Settings => Msg::Settings(match error {
            Some(why) => json!({"error": why}),
            None => result,
        }),
        Pending::SettingsSet => Msg::SettingResult(match error {
            Some(why) => json!({"outcome": "refused", "reason": why}),
            None => result,
        }),
        Pending::Proposals => Msg::Proposals(match error {
            Some(_) => json!({"proposals": []}),
            None => result,
        }),
        Pending::Decide => Msg::Decided(match error {
            Some(why) => json!({"ok": false, "text": why}),
            None => result,
        }),
        Pending::Facts => match error {
            Some(why) => Msg::FactsError(why),
            None => Msg::Facts(serde_json::from_value(result["facts"].take()).unwrap_or_default()),
        },
        Pending::Activity {
            grants,
            append,
            generation,
        } => Msg::Activity {
            grants,
            append,
            generation,
            answer: match (v["error"]["code"].as_i64(), error) {
                (Some(METHOD_NOT_FOUND), _) => json!({"entries": [], "next": null, "old": true}),
                (_, Some(why)) => json!({"error": why}),
                _ => result,
            },
        },
    })
}

/// One line from the daemon, every string in it (keys too) through [`super::view::safe`]: the
/// one place the agent's and the daemon's text is cleaned, so every event, answer and the
/// handshake reach the widgets, the copied code blocks and `/copy` already clean.
fn parse(line: &str) -> Option<Value> {
    fn clean(v: &mut Value) {
        match v {
            Value::String(s) => *s = super::view::safe(s),
            Value::Array(a) => a.iter_mut().for_each(clean),
            Value::Object(m) => {
                *m = (std::mem::take(m).into_iter())
                    .map(|(k, mut v)| {
                        clean(&mut v);
                        (super::view::safe(&k), v)
                    })
                    .collect();
            }
            _ => {}
        }
    }
    let mut v = serde_json::from_str(line).ok()?;
    clean(&mut v);
    Some(v)
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
            let Some(mut v) = parse(&line) else {
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

    async fn prompt(
        &mut self,
        text: &str,
        after: bool,
        carry: &mut Option<(String, bool)>,
    ) -> Result<(), Fail> {
        let command = json!({"type": "prompt", "content": text, "followUp": after});
        let params = json!({"sessionId": "main", "command": command});
        if let Err(e) = self.request("send", params).await {
            *carry = Some((text.to_string(), after));
            return Err(e.into());
        }
        Ok(())
    }
}

/// The tests' scripted attach daemon: answers the handshake, proving itself with `token`
/// and refusing a client that cannot, so a test can then script whatever the panel asks for.
/// Shared with `tui::mod`'s tests, which drive the same fake to a login panel.
#[cfg(test)]
pub(crate) async fn handshake(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
    write: &mut OwnedWriteHalf,
    token: &str,
    model: &str,
) -> Option<()> {
    use crate::attach::{proof, proven, BY_CLIENT, BY_DAEMON};
    let mut nonces = None;
    for _ in 0..4 {
        let req: Value = serde_json::from_str(&lines.next_line().await.ok()??).ok()?;
        let id = &req["id"];
        let answer = match req["method"].as_str()? {
            "zephyr.challenge" => {
                let hex = req["params"]["nonce"].as_str()?;
                let client = data_encoding::HEXLOWER.decode(hex.as_bytes()).ok()?;
                let (daemon, hex) = crate::attach::nonce();
                let proof = proof(token, BY_DAEMON, &client, &daemon);
                nonces = Some((client, daemon));
                json!({"jsonrpc": "2.0", "id": id, "result": {"nonce": hex, "proof": proof}})
            }
            "zephyr.prove" => {
                let (client, daemon) = nonces.take()?;
                let given = req["params"]["proof"].as_str()?;
                match proven(token, BY_CLIENT, &client, &daemon, given) {
                    true => json!({"jsonrpc": "2.0", "id": id, "result": {"authenticated": true}}),
                    false => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "invalid proof"}})
                    }
                }
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

    #[test]
    fn a_fact_value_is_counted_not_printed() {
        let ask = Ask::FactsPut(Box::new(crate::facts::Fact {
            name: "test-pin".into(),
            kind: crate::facts::FactKind::Digits,
            value: "135798".into(),
            allow: vec![],
        }));
        let shown = format!("{ask:?}");
        assert!(!shown.contains("135798"), "{shown}");
        assert!(
            shown.contains("test-pin") && shown.contains("value: 6"),
            "{shown}"
        );
    }

    /// The facts calls carry the value only in `facts_put`, and come back as the list.
    #[tokio::test]
    async fn facts_are_listed_saved_and_removed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "t", "m").await.unwrap();
            let mut asked = vec![];
            for _ in 0..3 {
                let req: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let answer = match req["method"].as_str().unwrap() {
                    "zephyr.facts_rm" => json!({"jsonrpc": "2.0", "id": req["id"],
                        "error": {"code": -32602, "message": "no fact named gone"}}),
                    _ => json!({"jsonrpc": "2.0", "id": req["id"], "result": {"facts": [
                        {"name": "test-pin", "kind": "digits", "allow": []}]}}),
                };
                w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
                asked.push(req);
            }
            asked
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "t".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(Msg::Up { .. })));
        ptx.send(Ask::FactsList).unwrap();
        ptx.send(Ask::FactsPut(Box::new(crate::facts::Fact {
            name: "test-pin".into(),
            kind: crate::facts::FactKind::Digits,
            value: "135798".into(),
            allow: vec!["a.example.com".into()],
        })))
        .unwrap();
        ptx.send(Ask::FactsRm("gone".into())).unwrap();
        let listed = FactEntry {
            name: "test-pin".into(),
            kind: "digits".into(),
            allow: vec![],
        };
        for _ in 0..2 {
            match rx.recv().await {
                Some(Msg::Facts(list)) => assert_eq!(list, std::slice::from_ref(&listed)),
                other => panic!("{other:?}"),
            }
        }
        match rx.recv().await {
            Some(Msg::FactsError(why)) => assert_eq!(why, "no fact named gone"),
            other => panic!("{other:?}"),
        }
        let asked = server.await.unwrap();
        assert_eq!(asked[0]["method"], "zephyr.facts_list");
        assert_eq!(
            asked[1]["params"],
            json!({"name": "test-pin", "kind": "digits", "value": "135798",
                   "allow": ["a.example.com"]})
        );
        assert_eq!(asked[2]["params"], json!({"name": "gone"}));
    }

    /// The agent's text reaches the owner's terminal only cleaned: an OSC 52 that would set the
    /// clipboard, a screen clear, a C1 control and a bidi override never get past `conn`, in an
    /// event, in the handshake's messages or in an answer.
    #[tokio::test]
    async fn escapes_and_bidi_controls_never_reach_the_view() {
        const EVIL: &str = "\x1b]52;c;cm0gLXJmIH4=\x07hi \u{202e}evil\x1b[2J\u{9b}31m\r\nok\tdone";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            handshake(&mut lines, &mut w, "tok", EVIL).await.unwrap();
            let message = json!({"role": "assistant", "content": [
                {"type": "text", "text": EVIL}, {"type": "thinking", "thinking": EVIL}]});
            let events = [
                json!({"type": "message_end", "message": message}),
                json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": EVIL,
                       "args": {"command": EVIL}}),
                json!({"type": "agent_settled", "error": EVIL}),
            ];
            for event in events {
                let ev = json!({"jsonrpc": "2.0", "method": "event",
                                "params": {"subscriptionId": 1, "event": event}});
                w.write_all(format!("{ev}\n").as_bytes()).await.unwrap();
            }
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let answer = json!({"jsonrpc": "2.0", "id": req["id"], "error": {"message": EVIL}});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            lines.next_line().await.ok();
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        tokio::spawn(run(addr, "tok".into(), tx, prx));
        let bad = |s: &str| {
            s.contains("\x1b]") || s.contains("\x1b[2J") || s.contains(['\u{202e}', '\u{9b}', '\r'])
        };
        let Some(Msg::Up { snapshot, .. }) = rx.recv().await else {
            panic!("no Up")
        };
        let model = snapshot.model.unwrap_or_default();
        assert!(!bad(&model) && model.contains("hi evil"), "{model:?}");
        let mut view =
            super::super::view::View::new(super::super::theme::DARK, jiff::tz::TimeZone::UTC);
        for _ in 0..3 {
            let Some(Msg::Event(ev)) = rx.recv().await else {
                panic!("no event")
            };
            assert!(
                !bad(&ev.to_string()) && ev.to_string().contains("ok\\tdone"),
                "{ev}"
            );
            view.apply(&ev);
        }
        let drawn = rpi_tui::Component::render(&*view.root, 100).join("\n");
        assert!(!bad(&drawn), "{drawn:?}");
        assert!(drawn.contains("evil"), "{drawn:?}");
        ptx.send(Ask::SettingsSet {
            key: "k".into(),
            value: "v".into(),
            until: None,
        })
        .unwrap();
        let Some(Msg::SettingResult(v)) = rx.recv().await else {
            panic!("no answer")
        };
        assert!(!bad(&v.to_string()), "{v}");
    }

    /// Whatever listens on the attach address (the agent's program, while the daemon is down)
    /// and agrees to everything never hears the token, and the TUI stops there, saying why.
    #[tokio::test]
    async fn a_listener_that_cannot_prove_itself_never_gets_the_token() {
        const TOKEN: &str = "s3cret-attach-token";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let heard = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            let mut heard = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                heard.push_str(&line);
                let req: Value = serde_json::from_str(&line).unwrap_or_default();
                // An impostor's best: yes to anything, with a proof it cannot make.
                let answer = json!({"jsonrpc": "2.0", "id": req["id"], "result": {
                    "authenticated": true, "nonce": "00".repeat(32), "proof": "00".repeat(32)}});
                if w.write_all(format!("{answer}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
            heard
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ptx, prx) = mpsc::unbounded_channel();
        let on = run(addr, TOKEN.into(), tx, prx);
        let gone = tokio::time::timeout(Duration::from_secs(10), on).await;
        assert!(gone.is_ok(), "the TUI stayed on the impostor");
        match rx.recv().await {
            Some(Msg::Fatal(e)) => assert!(e.contains("could not prove"), "{e}"),
            other => panic!("{other:?}"),
        }
        let heard = heard.await.unwrap();
        assert!(
            !heard.contains(TOKEN),
            "the token reached the listener: {heard}"
        );
        assert!(heard.contains("zephyr.challenge"), "{heard}");
    }

    /// An impostor that answers the challenge with nothing, or with an error, is no daemon
    /// either: the TUI ends with the same words, and never sends a prove.
    #[tokio::test]
    async fn an_impostor_that_refuses_the_challenge_is_fatal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let heard = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            let mut heard = vec![];
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).unwrap_or_default();
                heard.push(req["method"].as_str().unwrap_or_default().to_string());
                let answer = json!({"jsonrpc": "2.0", "id": req["id"], "result": {}});
                if w.write_all(format!("{answer}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
            heard
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ptx, prx) = mpsc::unbounded_channel();
        let on = run(addr, "tok".into(), tx, prx);
        let gone = tokio::time::timeout(Duration::from_secs(10), on).await;
        assert!(gone.is_ok(), "the TUI stayed on the impostor");
        assert!(matches!(rx.recv().await, Some(Msg::Fatal(e)) if e.contains("could not prove")));
        assert_eq!(heard.await.unwrap(), ["zephyr.challenge"]);
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
        assert!(matches!(rx.recv().await, Some(Msg::Fatal(e)) if e.contains("could not prove")));
    }

    #[test]
    fn a_saved_vault_entry_card_says_what_it_may_do() {
        let login = VaultEntry {
            kind: "login".into(),
            name: "github".into(),
            site: "https://github.com".into(),
            username: "me".into(),
            totp: true,
            ..Default::default()
        };
        assert_eq!(
            login.saved(false),
            "saved the login github\nsite  https://github.com\nuser  me\n\
             code  one-time codes from the saved seed\nfill  the first one waits for your OK\n\
             the agent was told it can use it now"
        );
        let token = VaultEntry {
            kind: "token".into(),
            name: "GH_TOKEN".into(),
            programs: vec!["gh".into()],
            ..Default::default()
        };
        assert_eq!(
            token.saved(true),
            "updated the token GH_TOKEN\nuse   allowed for gh\n\
             host  bound to no host yet: /vault bind GH_TOKEN HOST.."
        );
        let bound = VaultEntry {
            hosts: vec!["api.github.com:443".into(), "x.test:8443".into()],
            ..token.clone()
        };
        assert!(bound
            .saved(true)
            .ends_with("\nhost  sent only to api.github.com, x.test:8443"));
        let raw = VaultEntry { raw: true, ..bound };
        assert!(raw
            .saved(true)
            .ends_with("\nhost  handed out as its real value (raw)"));
    }
}
