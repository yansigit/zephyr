//! iMessage channel through a BlueBubbles Server on a Mac. See DESIGN.md sections 4.2 and 4.3.
//!
//! Inbound: BlueBubbles POSTs `{type, data}` webhooks to our listener. The secret rides in the
//! registered URL's query string, because BlueBubbles sends no custom headers.
//! Outbound: `POST /api/v1/message/text` with the server password as a query param.

use crate::channel::{Activity, Channel, Inbound, Outbound};
use anyhow::Context as _;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// Chat style 43 is a group chat (BlueBubbles `Chat.isGroup`). v1 ignores them, as Discord guilds are ignored.
const GROUP_STYLE: i64 = 43;
/// Largest webhook body read. BlueBubbles caps the serialized new-message payload at 4000 bytes for webhooks.
const MAX_BODY: usize = 64 * 1024;
/// Webhook connections served at once; one more is closed as it arrives.
const MAX_CONNS: usize = 32;
/// How long a webhook connection has to send its request head.
const HEAD_WAIT: Duration = Duration::from_secs(10);
/// shortcut: a fixed 60 s guess for a send that waits on AppleScript; not measured, raise it if real sends time out.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// Wait before retrying a failed backfill, e.g. while the Mac is asleep.
const BACKFILL_RETRY: Duration = Duration::from_secs(30);
/// How often an active typing indicator is sent again, so it stays up while the reply is worked on.
const TYPING_REFRESH: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    /// Webhook listener address. Intended to be this host's Tailscale IP, so the Mac can reach it.
    pub listen: SocketAddr,
    /// Permit an unspecified `listen` (0.0.0.0 or ::). Off by default: it exposes the listener on every interface.
    #[serde(default)]
    pub allow_all_interfaces: bool,
    /// BlueBubbles server URL, e.g. `http://100.64.1.2:1234`.
    pub server_url: String,
    /// Env var holding the BlueBubbles server password. Never stored in config or logged.
    pub password_env: String,
    /// Env var holding the webhook shared secret, which goes in the registered webhook URL as `?secret=`.
    pub secret_env: String,
    /// How far back to re-read BlueBubbles when the adapter starts. Older messages missed during downtime are lost.
    #[serde(default = "default_lookback_hours")]
    pub lookback_hours: u64,
    /// Most messages re-read at start. BlueBubbles rejects a query limit above 1000.
    #[serde(default = "default_backfill_limit")]
    pub backfill_limit: u32,
    /// Show a typing indicator while a reply is worked on. Needs the BlueBubbles Private API on the Mac.
    #[serde(default)]
    pub typing: bool,
}

fn default_lookback_hours() -> u64 {
    24
}

fn default_backfill_limit() -> u32 {
    200
}

#[derive(Clone)]
pub struct IMessage {
    listen: SocketAddr,
    server_url: String,
    password: String,
    secret: String,
    lookback: Duration,
    backfill_limit: u32,
    http: reqwest::Client,
    typing: bool,
    /// The refresh task for each recipient whose indicator is on. Clones share it, so any clone can stop it.
    typing_tasks: Arc<Mutex<HashMap<String, AbortHandle>>>,
    /// Set by the first typing error of the run, so later ones are not logged.
    typing_warned: Arc<AtomicBool>,
    typing_refresh: Duration,
    /// Owners and home, for saving the files an owner sends (`inbox`).
    live: crate::inbox::Live,
}

/// Refuses an unspecified bind address unless the operator said so.
fn check_listen(listen: SocketAddr, allow_all: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        allow_all || !listen.ip().is_unspecified(),
        "imessage listen {listen} binds all interfaces; bind the Tailscale IP or set allow_all_interfaces"
    );
    Ok(())
}

/// A limit above 1000 would make every backfill fail with a 400, so refuse it up front.
fn check_backfill_limit(limit: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=1000).contains(&limit),
        "imessage backfill_limit {limit} must be 1 to 1000"
    );
    Ok(())
}

/// A `/message/query` response. `data` holds raw message rows, newest first.
#[derive(Deserialize)]
struct Page {
    data: Vec<serde_json::Value>,
}

/// The parts of a BlueBubbles webhook envelope `{type, data}`. `data` stays raw until we know the type.
#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    data: serde_json::Value,
}

/// The fields of a BlueBubbles message that we read. Field names follow its serializer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    guid: String,
    text: Option<String>,
    is_from_me: bool,
    handle: Option<Handle>,
    chats: Option<Vec<Chat>>,
    associated_message_guid: Option<String>,
    item_type: Option<i64>,
    attachments: Option<Vec<Attachment>>,
}

/// A file on a message, fetched from `/api/v1/attachment/{guid}/download`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attachment {
    guid: String,
    mime_type: Option<String>,
    transfer_name: Option<String>,
    total_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct Handle {
    address: String,
    /// `iMessage` or `SMS`. An SMS sender number is trivial to spoof, so only iMessage counts;
    /// every BlueBubbles message we have recorded names its service.
    service: Option<String>,
}

#[derive(Deserialize)]
struct Chat {
    style: Option<i64>,
}

/// Maps a BlueBubbles webhook body to an inbound row and its files. `Ok(None)` means a valid event we ignore.
fn to_inbound(body: &[u8]) -> anyhow::Result<Option<(Inbound, Vec<Attachment>)>> {
    let event: Event = serde_json::from_slice(body)?;
    if event.kind != "new-message" {
        return Ok(None);
    }
    Ok(message_to_inbound(serde_json::from_value(event.data)?))
}

/// The filter shared by webhooks and backfill. `None` means a message we ignore.
fn message_to_inbound(m: Message) -> Option<(Inbound, Vec<Attachment>)> {
    let group = m
        .chats
        .as_deref()
        .and_then(|c| c.first())
        .and_then(|c| c.style)
        == Some(GROUP_STYLE);
    // Tapbacks are their own message rows that point at the target message. item_type != 0 is a group event.
    let tapback = m
        .associated_message_guid
        .as_deref()
        .is_some_and(|g| !g.is_empty());
    if m.is_from_me || group || tapback || m.item_type.unwrap_or(0) != 0 {
        return None;
    }
    let files = m.attachments.unwrap_or_default();
    // iMessage holds each attachment's place in the text with U+FFFC.
    let text = m.text.unwrap_or_default().replace('\u{fffc}', "");
    let (Some(handle), false) = (m.handle, text.is_empty() && files.is_empty()) else {
        return None;
    };
    if handle.service.as_deref() != Some("iMessage") {
        return None;
    }
    let inbound = Inbound {
        channel: "imessage".into(),
        platform_msg_id: m.guid,
        sender: handle.address,
        text,
    };
    Some((inbound, files))
}

/// Queues messages from the last `lookback` that a webhook may have missed. The router dedupes on guid.
async fn backfill(this: &IMessage, tx: &mpsc::Sender<Inbound>) -> anyhow::Result<()> {
    let since = crate::unix_millis().saturating_sub(this.lookback.as_millis()) as u64;
    let body = serde_json::json!({
        "after": since,
        "sort": "DESC",
        "limit": this.backfill_limit,
        "with": ["chats", "attachment"],
    });
    let page: Page = serde_json::from_slice(&this.post("/api/v1/message/query", &body).await?)?;
    // The query is newest first. Queue oldest first so the session sees messages in order.
    for raw in page.data.into_iter().rev() {
        // A row that does not parse is skipped, so one odd row cannot block the rest.
        let Ok(m) = serde_json::from_value::<Message>(raw) else {
            continue;
        };
        if let Some((inbound, files)) = message_to_inbound(m) {
            if tx
                .send(this.with_files(inbound, files).await)
                .await
                .is_err()
            {
                break;
            }
        }
    }
    Ok(())
}

fn status(code: StatusCode) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::default());
    *res.status_mut() = code;
    res
}

/// Handles one webhook request. The secret is checked before the body is read.
async fn webhook(req: Request<Incoming>, queue: Queue, secret: String) -> Response<Full<Bytes>> {
    if req.method() != Method::POST {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let given = req.uri().query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "secret")
            .map(|(_, v)| v.into_owned())
    });
    if !given.is_some_and(|g| bool::from(g.as_bytes().ct_eq(secret.as_bytes()))) {
        return status(StatusCode::UNAUTHORIZED);
    }
    let Ok(body) = Limited::new(req.into_body(), MAX_BODY).collect().await else {
        return status(StatusCode::BAD_REQUEST);
    };
    match to_inbound(&body.to_bytes()) {
        Ok(Some(row)) => {
            // A closed queue means run() is returning. BlueBubbles does not retry a webhook.
            let sent = queue.send(row).is_ok();
            status(if sent {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            })
        }
        Ok(None) => status(StatusCode::OK),
        Err(_) => status(StatusCode::BAD_REQUEST),
    }
}

/// Webhook rows waiting for their files, in arrival order.
type Queue = mpsc::UnboundedSender<(Inbound, Vec<Attachment>)>;

/// Accepts webhooks on `listener` until the process ends. One worker saves each row's files and
/// passes the rows on in arrival order, so a caption sent after a photo stays after it. The
/// webhook is answered once its row is queued: BlueBubbles gives up on one after 15 s and never
/// retries it, and the files can take longer. A crash meanwhile is covered by backfill.
async fn serve(listener: TcpListener, tx: mpsc::Sender<Inbound>, this: IMessage) {
    let (queue, mut rows) = mpsc::unbounded_channel();
    let secret = this.secret.clone();
    tokio::spawn(async move {
        while let Some((inbound, files)) = rows.recv().await {
            if tx
                .send(this.with_files(inbound, files).await)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    // Anything that reaches the port can connect (the agent too, under host networking): a cap,
    // so idle sockets cannot use up the daemon's file descriptors, and `HEAD_WAIT` for the head.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // shortcut: a persistent accept error (e.g. out of fds) retries every 100 ms with no backoff.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue; // dropped, so closed at once
        };
        let (queue, secret) = (queue.clone(), secret.clone());
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let (queue, secret) = (queue.clone(), secret.clone());
                async move { Ok::<_, Infallible>(webhook(req, queue, secret).await) }
            });
            // One request per connection, so the head deadline bounds an idle one too.
            let _ = http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(HEAD_WAIT)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), svc)
                .await;
            drop(slot);
        });
    }
}

fn redact(text: &str, password: &str) -> String {
    text.replace(password, "[redacted]")
}

/// Reads a required secret from the environment. The error names the variable, never its value.
fn env(name: &str) -> anyhow::Result<String> {
    let value = std::env::var(name).with_context(|| format!("env var {name} is not set"))?;
    anyhow::ensure!(!value.is_empty(), "env var {name} is empty");
    Ok(value)
}

impl IMessage {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        check_listen(cfg.listen, cfg.allow_all_interfaces)?;
        check_backfill_limit(cfg.backfill_limit)?;
        Ok(Self {
            listen: cfg.listen,
            server_url: cfg.server_url.trim_end_matches('/').to_string(),
            password: env(&cfg.password_env)?,
            secret: env(&cfg.secret_env)?,
            lookback: Duration::from_secs(cfg.lookback_hours * 3600),
            backfill_limit: cfg.backfill_limit,
            http: reqwest::Client::builder().timeout(SEND_TIMEOUT).build()?,
            typing: cfg.typing,
            typing_tasks: Default::default(),
            typing_warned: Default::default(),
            typing_refresh: TYPING_REFRESH,
            live: Default::default(),
        })
    }

    /// `inbound` with its files saved (an owner's only) and listed in its text.
    async fn with_files(&self, mut inbound: Inbound, files: Vec<Attachment>) -> Inbound {
        let files = files.into_iter().map(|a| {
            let guid: String = url::form_urlencoded::byte_serialize(a.guid.as_bytes()).collect();
            let url = format!("{}/api/v1/attachment/{guid}/download", self.server_url);
            crate::inbox::File {
                name: a.transfer_name.unwrap_or_else(|| "file".into()),
                mime: a.mime_type,
                size: a.total_bytes.unwrap_or(0),
                src: crate::inbox::Src::Get(
                    self.http.get(url).query(&[("password", &self.password)]),
                ),
            }
        });
        crate::inbox::append(&self.live, &mut inbound, files.collect()).await;
        inbound
    }

    /// Sets the typing indicator in the one-to-one chat with `to`. A no-op unless `[imessage] typing = true`.
    /// While on, it is re-sent every `TYPING_REFRESH` until it is set off or a reply goes to `to`.
    /// Errors are scrubbed. Callers ignore them, so typing never fails a reply.
    pub async fn set_typing(&self, to: &str, on: bool) -> anyhow::Result<()> {
        if !self.typing {
            return Ok(());
        }
        if !on {
            return self.stop_typing(to).await;
        }
        // The first send runs inside the refresh task, so a stop aborts it even while it is in flight.
        self.keep_refreshing(to);
        Ok(())
    }

    /// Whether typing can work: the Mac's BlueBubbles Private API is on and its helper is connected.
    pub async fn private_api_on(&self) -> anyhow::Result<bool> {
        let body = self
            .request(Method::GET, "/api/v1/server/info", Payload::None)
            .await?;
        let info: serde_json::Value = serde_json::from_slice(&body)?;
        let on = |key: &str| info["data"][key].as_bool() == Some(true);
        Ok(on("private_api") && on("helper_connected"))
    }

    /// Starts the refresh for `to`, unless one is running already.
    fn keep_refreshing(&self, to: &str) {
        let mut tasks = self.typing_tasks.lock().unwrap_or_else(|e| e.into_inner());
        if tasks.contains_key(to) {
            return;
        }
        let (me, chat) = (self.clone(), to.to_string());
        let task = tokio::spawn(async move {
            loop {
                if let Err(e) = me.typing_request(Method::POST, &chat).await {
                    me.typing_error(e);
                }
                tokio::time::sleep(me.typing_refresh).await;
            }
        });
        tasks.insert(to.to_string(), task.abort_handle());
    }

    /// Ends the refresh for `to` and clears its indicator. Nothing is sent if none was on.
    async fn stop_typing(&self, to: &str) -> anyhow::Result<()> {
        let Some(task) = self.take_refresh(to) else {
            return Ok(());
        };
        task.abort();
        self.typing_request(Method::DELETE, to)
            .await
            .map_err(|e| self.typing_error(e))
    }

    fn take_refresh(&self, to: &str) -> Option<AbortHandle> {
        self.typing_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(to)
    }

    async fn typing_request(&self, method: Method, to: &str) -> anyhow::Result<()> {
        self.request(method, &typing_path(&chat_guid(to)), Payload::None)
            .await
            .map(drop)
    }

    /// Logs the run's first typing error, already scrubbed. Later ones are dropped, so a dead Private API cannot flood the log.
    fn typing_error(&self, e: anyhow::Error) -> anyhow::Error {
        if !self.typing_warned.swap(true, Ordering::Relaxed) {
            eprintln!("zephyr: imessage typing failed, later typing errors are not logged: {e:#}");
        }
        e
    }
}

/// The chat GUID of the one-to-one iMessage chat with `handle`.
fn chat_guid(handle: &str) -> String {
    format!("iMessage;-;{handle}")
}

/// The typing route for a chat. The GUID is one percent-encoded path segment, which the server decodes.
fn typing_path(chat_guid: &str) -> String {
    // shortcut: byte_serialize writes a space as '+', which no iMessage handle contains.
    let guid: String = url::form_urlencoded::byte_serialize(chat_guid.as_bytes()).collect();
    format!("/api/v1/chat/{guid}/typing")
}

impl Channel for IMessage {
    fn name(&self) -> &str {
        "imessage"
    }

    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        // Under the network floor `listen` is where the host publishes the webhook; the daemon
        // itself binds the container's own address (DESIGN §4.20).
        let addr = crate::egress::listen_addr(self.listen)?;
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        // Backfill runs beside the listener, so webhooks that arrive meanwhile are not missed. A failure
        // (Mac asleep, server down) is retried. It must not end run(), because the router ends the gateway then.
        let backfill_loop = async {
            loop {
                match backfill(self, &tx).await {
                    Ok(()) => std::future::pending::<()>().await,
                    Err(e) => {
                        eprintln!("zephyr: imessage backfill failed, retrying: {e:#}");
                        tokio::time::sleep(BACKFILL_RETRY).await;
                    }
                }
            }
        };
        tokio::select! {
            _ = serve(listener, tx.clone(), self.clone()) => {}
            _ = backfill_loop => {}
            _ = tx.closed() => {}
        }
        anyhow::bail!("imessage adapter stopped")
    }

    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        // The indicator clears before the reply, so it never outlasts it. A failure here does not block the reply.
        let _ = self.set_typing(&msg.recipient, false).await;
        let body = serde_json::json!({
            // One-to-one iMessage chat. BlueBubbles maps the `iMessage` prefix to `any` on macOS Tahoe.
            "chatGuid": chat_guid(&msg.recipient),
            "message": msg.text,
            "method": "apple-script",
            "tempGuid": msg.client_id,
        });
        if !msg.text.is_empty() || msg.attachment.is_none() {
            self.post("/api/v1/message/text", &body).await?;
        }
        if let Some(path) = &msg.attachment {
            let name = path
                .file_name()
                .map_or("file".into(), |n| n.to_string_lossy().into_owned());
            let file = tokio::fs::read(path)
                .await
                .context("reading the file to send")?;
            let form = reqwest::multipart::Form::new()
                .text("chatGuid", chat_guid(&msg.recipient))
                .text("tempGuid", format!("{}-f", msg.client_id))
                .text("name", name.clone())
                .text("method", "apple-script")
                .part(
                    "attachment",
                    reqwest::multipart::Part::bytes(file).file_name(name),
                );
            self.request(
                Method::POST,
                "/api/v1/message/attachment",
                Payload::Form(form),
            )
            .await?;
        }
        Ok(())
    }

    fn live(&self, cfg: tokio::sync::watch::Receiver<Arc<crate::config::Config>>) {
        let _ = self.live.set(cfg);
    }

    /// Typing on Start, cleared on Stop. Progress shows nothing: the refresh task keeps typing up.
    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        match a {
            Activity::Start => self.set_typing(to, true).await,
            Activity::Stop => self.set_typing(to, false).await,
            Activity::Progress { .. } => Ok(()),
        }
    }
}

/// What a BlueBubbles request carries.
enum Payload<'a> {
    None,
    Json(&'a serde_json::Value),
    /// An attachment upload.
    Form(reqwest::multipart::Form),
}

impl IMessage {
    /// POSTs JSON to `path` with the password as a query param, and returns the response body.
    async fn post(&self, path: &str, body: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
        self.request(Method::POST, path, Payload::Json(body)).await
    }

    /// Sends `method` to `path` with the password as a query param, and returns the response body.
    /// Every error is scrubbed of the password, which is in the URL.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Payload<'_>,
    ) -> anyhow::Result<Vec<u8>> {
        let req = self
            .http
            .request(method, format!("{}{path}", self.server_url))
            .query(&[("password", &self.password)]);
        let req = match body {
            Payload::None => req,
            Payload::Json(body) => req.json(body),
            Payload::Form(form) => req.multipart(form),
        };
        let res = req
            .send()
            .await
            .map_err(|e| self.scrub("sending to BlueBubbles failed", e))?;
        let status = res.status();
        let bytes = res
            .bytes()
            .await
            .map_err(|e| self.scrub("reading the BlueBubbles response failed", e))?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&bytes);
            anyhow::bail!(
                "BlueBubbles returned {status}: {}",
                redact(&text, &self.password)
            );
        }
        Ok(bytes.to_vec())
    }

    /// The error with its cause chain, minus the URL and the password.
    fn scrub(&self, what: &str, e: reqwest::Error) -> anyhow::Error {
        let text = format!("{:#}", anyhow::Error::new(e.without_url()));
        anyhow::anyhow!("{what}: {}", redact(&text, &self.password))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW: &str = include_str!("../tests/fixtures/bluebubbles/new-message.json");
    const FROM_ME: &str = include_str!("../tests/fixtures/bluebubbles/from-me.json");
    const GROUP: &str = include_str!("../tests/fixtures/bluebubbles/group-chat.json");
    const TAPBACK: &str = include_str!("../tests/fixtures/bluebubbles/tapback.json");
    const TYPING: &str = include_str!("../tests/fixtures/bluebubbles/typing-indicator.json");

    #[test]
    fn maps_new_message_to_inbound() {
        let (got, files) = to_inbound(NEW.as_bytes()).unwrap().unwrap();
        assert!(files.is_empty());
        assert_eq!(
            got,
            Inbound {
                channel: "imessage".into(),
                platform_msg_id: "AAAA-1111-2222".into(),
                sender: "+15551234567".into(),
                text: "hello from my phone".into(),
            }
        );
    }

    #[test]
    fn ignores_from_me_group_tapback_and_other_events() {
        assert!(to_inbound(FROM_ME.as_bytes()).unwrap().is_none(), "from-me");
        assert!(
            to_inbound(GROUP.as_bytes()).unwrap().is_none(),
            "group chat"
        );
        assert!(to_inbound(TAPBACK.as_bytes()).unwrap().is_none(), "tapback");
        assert!(
            to_inbound(TYPING.as_bytes()).unwrap().is_none(),
            "typing indicator"
        );
        let updated = NEW.replace("new-message", "updated-message");
        assert!(
            to_inbound(updated.as_bytes()).unwrap().is_none(),
            "updated-message"
        );
        // An SMS, whose sender number anyone can spoof, or a handle naming no service.
        let sms = NEW.replace(r#""service": "iMessage""#, r#""service": "SMS""#);
        assert_ne!(sms, NEW);
        assert!(to_inbound(sms.as_bytes()).unwrap().is_none(), "sms");
        let bare = NEW.replace(r#", "service": "iMessage""#, "");
        assert_ne!(bare, NEW);
        assert!(to_inbound(bare.as_bytes()).unwrap().is_none(), "no service");
    }

    #[test]
    fn ignores_empty_text_and_rejects_garbage() {
        let empty = NEW.replace("hello from my phone", "");
        assert!(to_inbound(empty.as_bytes()).unwrap().is_none());
        assert!(to_inbound(b"not json").is_err());
    }

    #[test]
    fn refuses_unspecified_bind_unless_allowed() {
        let any: SocketAddr = "0.0.0.0:8787".parse().unwrap();
        assert!(check_listen(any, false).is_err());
        assert!(check_listen("[::]:8787".parse().unwrap(), false).is_err());
        assert!(check_listen(any, true).is_ok());
        assert!(check_listen("100.64.1.2:8787".parse().unwrap(), false).is_ok());
    }

    /// Starts `serve` on an ephemeral 127.0.0.1 port. Returns the base URL and the inbound queue.
    async fn spawn_webhook(secret: &str) -> (String, mpsc::Receiver<Inbound>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel(8);
        let this = IMessage {
            secret: secret.into(),
            ..adapter(String::new())
        };
        tokio::spawn(serve(listener, tx, this));
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn webhook_with_secret_queues_message() {
        let (base, mut rx) = spawn_webhook("s3cret").await;
        let res = reqwest::Client::new()
            .post(format!("{base}/hook?secret=s3cret"))
            .header("content-type", "application/json")
            .body(NEW)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(rx.recv().await.unwrap().platform_msg_id, "AAAA-1111-2222");
    }

    #[tokio::test]
    async fn idle_connections_cannot_crowd_out_a_webhook() {
        use tokio::io::AsyncReadExt;
        let (base, mut rx) = spawn_webhook("s3cret").await;
        let addr = base.trim_start_matches("http://").to_string();
        let mut idle = Vec::new();
        for _ in 0..40 {
            idle.push(tokio::net::TcpStream::connect(&addr).await.unwrap());
        }
        // Past the cap a connection is closed as it arrives; the rest once 10 s pass with no head.
        let started = std::time::Instant::now();
        loop {
            assert!(started.elapsed() < Duration::from_secs(20), "never let in");
            let sent = reqwest::Client::new()
                .post(format!("{base}/hook?secret=s3cret"))
                .body(NEW)
                .send()
                .await;
            if sent.is_ok_and(|r| r.status() == 200) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(rx.recv().await.unwrap().platform_msg_id, "AAAA-1111-2222");
        for mut s in idle {
            let read = tokio::time::timeout(Duration::from_secs(1), s.read(&mut [0u8; 64])).await;
            assert!(
                matches!(read, Ok(Ok(0) | Err(_))),
                "an idle socket is still open"
            );
        }
    }

    #[tokio::test]
    async fn webhook_with_wrong_or_missing_secret_is_refused() {
        let (base, mut rx) = spawn_webhook("s3cret").await;
        let client = reqwest::Client::new();
        for url in [format!("{base}/hook?secret=nope"), format!("{base}/hook")] {
            let res = client.post(url).body(NEW).send().await.unwrap();
            assert_eq!(res.status(), 401);
        }
        assert!(
            rx.try_recv().is_err(),
            "nothing may be queued without the secret"
        );
    }

    /// Captures each request's `METHOD path?query` and body, then answers with `reply`.
    async fn stub(status: u16, reply: &'static str) -> (String, mpsc::Receiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (cap_tx, cap_rx) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let cap_tx = cap_tx.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let cap_tx = cap_tx.clone();
                        async move {
                            let uri = format!("{} {}", req.method(), req.uri());
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let file = uri.starts_with("GET /api/v1/attachment/");
                            let _ = cap_tx
                                .send((uri, String::from_utf8_lossy(&body).into()))
                                .await;
                            if file {
                                // BlueBubbles serves a HEIC photo converted to JPEG, slowly.
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                return Ok(Response::builder()
                                    .header("content-type", "image/jpeg")
                                    .body(Full::new(Bytes::from_static(b"jpeg!")))
                                    .unwrap());
                            }
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .status(status)
                                    .body(Full::new(Bytes::from(reply)))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        (format!("http://{addr}"), cap_rx)
    }

    fn adapter(server_url: String) -> IMessage {
        IMessage {
            listen: "127.0.0.1:0".parse().unwrap(),
            server_url,
            password: "pw-hunter2".into(),
            secret: "s3cret".into(),
            lookback: Duration::from_secs(24 * 3600),
            backfill_limit: 200,
            http: reqwest::Client::new(),
            typing: false,
            typing_tasks: Default::default(),
            typing_warned: Default::default(),
            typing_refresh: TYPING_REFRESH,
            live: Default::default(),
        }
    }

    /// The next request a `stub` saw. Fails the test instead of hanging when none arrives.
    async fn next(cap: &mut mpsc::Receiver<(String, String)>) -> (String, String) {
        tokio::time::timeout(Duration::from_secs(5), cap.recv())
            .await
            .expect("no request arrived")
            .expect("stub closed")
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// The `data` object of a fixture, with its guid replaced. Used to build query rows.
    fn message_data(fixture: &str, guid: &str) -> serde_json::Value {
        let mut event: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let mut data = event["data"].take();
        data["guid"] = guid.into();
        data
    }

    #[test]
    fn backfill_limit_is_bounded_by_the_server() {
        assert!(check_backfill_limit(0).is_err());
        assert!(check_backfill_limit(1).is_ok());
        assert!(check_backfill_limit(1000).is_ok());
        assert!(check_backfill_limit(1001).is_err());
    }

    #[test]
    fn config_defaults_to_a_day_and_200_messages() {
        let cfg: Config = toml::from_str(
            r#"
            listen = "127.0.0.1:8787"
            server_url = "http://127.0.0.1:1234"
            password_env = "P"
            secret_env = "S"
            "#,
        )
        .unwrap();
        assert_eq!((cfg.lookback_hours, cfg.backfill_limit), (24, 200));
    }

    #[tokio::test]
    async fn backfill_queries_recent_messages_and_queues_oldest_first() {
        // BlueBubbles returns newest first, so the list below is [newer, older].
        let reply = serde_json::json!({
            "status": 200,
            "data": [message_data(NEW, "BBBB-newer"), message_data(NEW, "AAAA-older")],
        })
        .to_string();
        let (base, mut cap) = stub(200, leak(reply)).await;
        let (tx, mut rx) = mpsc::channel(8);
        let before = crate::unix_millis() as u64;
        backfill(&adapter(base), &tx).await.unwrap();

        let (uri, body) = cap.recv().await.unwrap();
        assert_eq!(uri, "POST /api/v1/message/query?password=pw-hunter2");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["sort"], "DESC");
        assert_eq!(json["limit"], 200);
        assert_eq!(json["with"], serde_json::json!(["chats", "attachment"]));
        let day = 24 * 3600 * 1000;
        let after = json["after"].as_u64().unwrap();
        assert!(before - day <= after && after <= crate::unix_millis() as u64 - day);

        assert_eq!(rx.recv().await.unwrap().platform_msg_id, "AAAA-older");
        assert_eq!(rx.recv().await.unwrap().platform_msg_id, "BBBB-newer");
    }

    #[tokio::test]
    async fn run_queues_backfill_and_keeps_listening() {
        let reply = serde_json::json!({ "status": 200, "data": [message_data(NEW, "AAAA-1")] });
        let (base, _cap) = stub(200, leak(reply.to_string())).await;
        // `run` binds 127.0.0.1:0 itself: a port picked here first could be taken by another test.
        let adapter = adapter(base);
        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn(async move { adapter.run(tx).await });
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.platform_msg_id, "AAAA-1");
        task.abort();
    }

    fn outbound() -> Outbound {
        Outbound {
            channel: "imessage".into(),
            recipient: "+15551234567".into(),
            client_id: "client-1".into(),
            text: "hi there".into(),
            attachment: None,
            reply_to: None,
        }
    }

    #[tokio::test]
    async fn send_posts_text_route_with_chat_guid_and_temp_guid() {
        let (base, mut cap) = stub(200, r#"{"status":200,"message":"Message sent!"}"#).await;
        adapter(base).send(&outbound()).await.unwrap();
        let (uri, body) = cap.recv().await.unwrap();
        assert_eq!(uri, "POST /api/v1/message/text?password=pw-hunter2");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["chatGuid"], "iMessage;-;+15551234567");
        assert_eq!(json["message"], "hi there");
        assert_eq!(json["method"], "apple-script");
        assert_eq!(json["tempGuid"], "client-1");
    }

    #[tokio::test]
    async fn send_errors_never_contain_the_password() {
        // The server echoes the password back in its error body.
        let (base, _cap) = stub(500, r#"{"error":"bad pw-hunter2"}"#).await;
        let err = adapter(base)
            .send(&outbound())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("500"), "{err}");
        assert!(!err.contains("pw-hunter2"), "{err}");

        // A dead port: reqwest's error text includes the URL, which carries the password.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        let err = adapter(format!("http://127.0.0.1:{port}"))
            .send(&outbound())
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains("pw-hunter2"), "{err}");
    }

    #[test]
    fn chat_guid_and_typing_path_address_a_dm_by_handle() {
        assert_eq!(chat_guid("+15551234567"), "iMessage;-;+15551234567");
        // One percent-encoded path segment. koa-router decodes it, so the server sees the raw GUID.
        assert_eq!(
            typing_path(&chat_guid("+15551234567")),
            "/api/v1/chat/iMessage%3B-%3B%2B15551234567/typing"
        );
    }

    #[test]
    fn typing_is_off_by_default() {
        let base = r#"
            listen = "127.0.0.1:8787"
            server_url = "http://127.0.0.1:1234"
            password_env = "P"
            secret_env = "S"
        "#;
        assert!(!toml::from_str::<Config>(base).unwrap().typing);
        let on: Config = toml::from_str(&format!("{base}\ntyping = true")).unwrap();
        assert!(on.typing);
    }

    #[tokio::test]
    async fn typing_is_a_noop_when_off() {
        let (base, mut cap) = stub(200, "{}").await;
        let im = adapter(base);
        im.set_typing("+15551234567", true).await.unwrap();
        im.set_typing("+15551234567", false).await.unwrap();
        assert!(
            cap.try_recv().is_err(),
            "typing off must not call BlueBubbles"
        );
    }

    #[tokio::test]
    async fn reply_stops_typing_before_it_is_sent() {
        let (base, mut cap) = stub(200, r#"{"status":200,"message":"ok"}"#).await;
        let im = IMessage {
            typing: true,
            ..adapter(base)
        };
        im.set_typing("+15551234567", true).await.unwrap();
        let (start, _) = next(&mut cap).await;
        im.send(&outbound()).await.unwrap();
        let (stop, _) = next(&mut cap).await;
        let (reply, _) = next(&mut cap).await;
        assert_eq!(
            start,
            "POST /api/v1/chat/iMessage%3B-%3B%2B15551234567/typing?password=pw-hunter2"
        );
        assert_eq!(
            stop,
            "DELETE /api/v1/chat/iMessage%3B-%3B%2B15551234567/typing?password=pw-hunter2"
        );
        assert_eq!(reply, "POST /api/v1/message/text?password=pw-hunter2");
    }

    #[tokio::test]
    async fn typing_is_refreshed_until_it_is_stopped() {
        let (base, mut cap) = stub(200, "{}").await;
        let im = IMessage {
            typing: true,
            typing_refresh: Duration::from_millis(20),
            ..adapter(base)
        };
        im.set_typing("+15551234567", true).await.unwrap();
        for _ in 0..3 {
            assert!(next(&mut cap).await.0.starts_with("POST /api/v1/chat/"));
        }
        im.set_typing("+15551234567", false).await.unwrap();

        // Whatever was queued before the stop is drained. The DELETE must be the last request.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut after = Vec::new();
        while let Ok((req, _)) = cap.try_recv() {
            after.push(req);
        }
        assert!(
            after.last().is_some_and(|r| r.starts_with("DELETE ")),
            "{after:?}"
        );
    }

    #[tokio::test]
    async fn typing_errors_never_contain_the_password() {
        // The server echoes the password back in its error body.
        let (base, _cap) = stub(500, r#"{"error":"bad pw-hunter2"}"#).await;
        let im = IMessage {
            typing: true,
            ..adapter(base)
        };
        let err = im
            .typing_request(Method::POST, "+15551234567")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("500"), "{err}");
        im.set_typing("+15551234567", true).await.unwrap();
        assert!(!err.contains("pw-hunter2"), "{err}");
        let err = im
            .set_typing("+15551234567", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains("pw-hunter2"), "{err}");

        // A dead port: reqwest's error text includes the URL, which carries the password.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        let im = IMessage {
            typing: true,
            ..adapter(format!("http://127.0.0.1:{port}"))
        };
        let err = im
            .typing_request(Method::POST, "+15551234567")
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains("pw-hunter2"), "{err}");
    }

    #[tokio::test]
    async fn private_api_is_read_from_server_info() {
        let on = r#"{"status":200,"message":"Success","data":{"private_api":true,"helper_connected":true}}"#;
        let (base, mut cap) = stub(200, on).await;
        assert!(adapter(base).private_api_on().await.unwrap());
        let (req, _) = next(&mut cap).await;
        assert_eq!(req, "GET /api/v1/server/info?password=pw-hunter2");

        let off = r#"{"status":200,"message":"Success","data":{"private_api":false}}"#;
        let (base, _cap) = stub(200, off).await;
        assert!(!adapter(base).private_api_on().await.unwrap());

        let no_helper = r#"{"status":200,"message":"Success","data":{"private_api":true,"helper_connected":false}}"#;
        let (base, _cap) = stub(200, no_helper).await;
        assert!(!adapter(base).private_api_on().await.unwrap());
    }

    const WITH_FILE: &str =
        include_str!("../tests/fixtures/bluebubbles/new-message-attachment.json");
    const OWNER: &str = "imessage = ['+15551234567']";

    #[test]
    fn a_message_that_is_only_a_file_is_kept() {
        let (got, files) = to_inbound(WITH_FILE.as_bytes()).unwrap().unwrap();
        assert_eq!(got.text, "", "U+FFFC placeholders are dropped");
        assert_eq!(files.len(), 1);
        let no_files = WITH_FILE.replace("\"attachments\": [", "\"x\": [");
        assert!(to_inbound(no_files.as_bytes()).unwrap().is_none());
    }

    #[tokio::test]
    async fn webhook_saves_an_owners_file_before_queueing() {
        let home = crate::test_dir("imessage-webhook-file");
        let (bb, mut cap) = stub(200, "{}").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let this = IMessage {
            live: crate::inbox::tests::live(&home, OWNER),
            ..adapter(bb)
        };
        tokio::spawn(serve(listener, tx, this));
        let res = reqwest::Client::new()
            .post(format!("http://{addr}/hook?secret=s3cret"))
            .body(WITH_FILE)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let path = home.join("inbox/imessage-CCCC-3333-4444-1.jpg");
        assert_eq!(
            got.text,
            format!(
                "[attached image: {} (IMG_4521.HEIC, image/jpeg, 1 KB); open it with read]",
                path.display()
            )
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"jpeg!");
        assert_eq!(
            next(&mut cap).await.0,
            "GET /api/v1/attachment/AT-0001-HEIC/download?password=pw-hunter2"
        );
    }

    #[tokio::test]
    async fn a_caption_sent_after_a_photo_stays_after_it() {
        let home = crate::test_dir("imessage-order");
        let (bb, _cap) = stub(200, "{}").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let this = IMessage {
            live: crate::inbox::tests::live(&home, OWNER),
            ..adapter(bb)
        };
        tokio::spawn(serve(listener, tx, this));
        let client = reqwest::Client::new();
        for body in [WITH_FILE, NEW] {
            let url = format!("http://{addr}/hook?secret=s3cret");
            let res = client.post(url).body(body).send().await.unwrap();
            assert_eq!(res.status(), 200);
        }
        assert_eq!(rx.recv().await.unwrap().platform_msg_id, "CCCC-3333-4444");
        assert_eq!(rx.recv().await.unwrap().platform_msg_id, "AAAA-1111-2222");
    }

    /// A query reply holding the recorded file message from `sender`.
    fn backfill_reply(sender: &str) -> &'static str {
        let mut data = message_data(WITH_FILE, "CCCC-3333-4444");
        data["handle"]["address"] = sender.into();
        leak(serde_json::json!({ "status": 200, "data": [data] }).to_string())
    }

    #[tokio::test]
    async fn backfill_saves_an_owners_file() {
        let home = crate::test_dir("imessage-backfill-file");
        let (bb, mut cap) = stub(200, backfill_reply("+15551234567")).await;
        let im = IMessage {
            live: crate::inbox::tests::live(&home, OWNER),
            ..adapter(bb)
        };
        let (tx, mut rx) = mpsc::channel(8);
        backfill(&im, &tx).await.unwrap();
        let got = rx.recv().await.unwrap();
        assert!(
            got.text.contains("imessage-CCCC-3333-4444-1.jpg"),
            "{}",
            got.text
        );
        assert!(next(&mut cap)
            .await
            .0
            .starts_with("POST /api/v1/message/query"));
        assert!(next(&mut cap)
            .await
            .0
            .starts_with("GET /api/v1/attachment/AT-0001-HEIC/download"));
    }

    #[tokio::test]
    async fn backfill_never_fetches_a_strangers_file() {
        let home = crate::test_dir("imessage-stranger");
        let (bb, mut cap) = stub(200, backfill_reply("+15550000000")).await;
        let im = IMessage {
            live: crate::inbox::tests::live(&home, OWNER),
            ..adapter(bb)
        };
        let (tx, mut rx) = mpsc::channel(8);
        backfill(&im, &tx).await.unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(
            got.text,
            "[attached file: IMG_4521.HEIC (image/heic, 1.7 MB, not downloaded)]"
        );
        assert!(next(&mut cap)
            .await
            .0
            .starts_with("POST /api/v1/message/query"));
        assert!(cap.try_recv().is_err(), "nothing is downloaded");
        assert!(!home.join("inbox").exists());
    }

    #[tokio::test]
    async fn only_ten_files_of_25_mb_or_less_are_fetched() {
        let home = crate::test_dir("imessage-limits");
        let (bb, mut cap) = stub(200, "{}").await;
        let im = IMessage {
            live: crate::inbox::tests::live(&home, OWNER),
            ..adapter(bb)
        };
        let mut event: serde_json::Value = serde_json::from_str(WITH_FILE).unwrap();
        let one = event["data"]["attachments"][0].clone();
        let files: Vec<_> = (1..=12)
            .map(|i| {
                let mut a = one.clone();
                a["guid"] = format!("AT-{i}").into();
                if i == 2 {
                    a["totalBytes"] = (crate::inbox::MAX_BYTES + 1).into();
                }
                a
            })
            .collect();
        event["data"]["attachments"] = files.into();
        let (inbound, files) = to_inbound(event.to_string().as_bytes()).unwrap().unwrap();
        let got = im.with_files(inbound, files).await;
        let lines: Vec<&str> = got.text.lines().collect();
        assert_eq!(lines.len(), 12);
        assert!(
            lines[1].ends_with("(image/heic, 25.0 MB, too large, not downloaded)]"),
            "{}",
            lines[1]
        );
        for line in &lines[10..] {
            assert!(line.ends_with("over 10 files, not downloaded)]"), "{line}");
        }
        let mut fetched = Vec::new();
        while let Ok((req, _)) = cap.try_recv() {
            fetched.push(req.split('/').nth(4).unwrap().to_string());
        }
        let want: Vec<String> = [1, 3, 4, 5, 6, 7, 8, 9, 10]
            .iter()
            .map(|i| format!("AT-{i}"))
            .collect();
        assert_eq!(fetched, want);
    }
}
