//! iMessage channel through a BlueBubbles Server on a Mac. See DESIGN.md sections 4.2 and 4.3.
//!
//! Inbound: BlueBubbles POSTs `{type, data}` webhooks to our listener. The secret rides in the
//! registered URL's query string, because BlueBubbles sends no custom headers.
//! Outbound: `POST /api/v1/message/text` with the server password as a query param.

use crate::channel::{Channel, Inbound, Outbound};
use anyhow::Context as _;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Chat style 43 is a group chat (BlueBubbles `Chat.isGroup`). v1 ignores them, as Discord guilds are ignored.
const GROUP_STYLE: i64 = 43;
/// Largest webhook body read. BlueBubbles caps the serialized new-message payload at 4000 bytes for webhooks.
const MAX_BODY: usize = 64 * 1024;
/// shortcut: a fixed 60 s guess for a send that waits on AppleScript; not measured, raise it if real sends time out.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize)]
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
}

pub struct IMessage {
    listen: SocketAddr,
    server_url: String,
    password: String,
    secret: String,
    http: reqwest::Client,
}

/// Refuses an unspecified bind address unless the operator said so.
fn check_listen(listen: SocketAddr, allow_all: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        allow_all || !listen.ip().is_unspecified(),
        "imessage listen {listen} binds all interfaces; bind the Tailscale IP or set allow_all_interfaces"
    );
    Ok(())
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
}

#[derive(Deserialize)]
struct Handle {
    address: String,
}

#[derive(Deserialize)]
struct Chat {
    style: Option<i64>,
}

/// Maps a BlueBubbles webhook body to an inbound row. `Ok(None)` means a valid event we ignore.
pub fn to_inbound(body: &[u8]) -> anyhow::Result<Option<Inbound>> {
    let event: Event = serde_json::from_slice(body)?;
    if event.kind != "new-message" {
        return Ok(None);
    }
    let m: Message = serde_json::from_value(event.data)?;
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
        return Ok(None);
    }
    let (Some(handle), Some(text)) = (m.handle, m.text.filter(|t| !t.is_empty())) else {
        return Ok(None);
    };
    Ok(Some(Inbound {
        channel: "imessage".into(),
        platform_msg_id: m.guid,
        sender: handle.address,
        text,
    }))
}

fn status(code: StatusCode) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::default());
    *res.status_mut() = code;
    res
}

/// Handles one webhook request. The secret is checked before the body is read.
async fn webhook(
    req: Request<Incoming>,
    tx: mpsc::Sender<Inbound>,
    secret: String,
) -> Response<Full<Bytes>> {
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
        Ok(Some(inbound)) => {
            // A closed queue means run() is returning. BlueBubbles does not retry a webhook.
            let sent = tx.send(inbound).await.is_ok();
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

/// Accepts webhooks on `listener` until the process ends.
async fn serve(listener: TcpListener, tx: mpsc::Sender<Inbound>, secret: String) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // shortcut: a persistent accept error (e.g. out of fds) retries every 100 ms with no backoff.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let (tx, secret) = (tx.clone(), secret.clone());
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let (tx, secret) = (tx.clone(), secret.clone());
                async move { Ok::<_, Infallible>(webhook(req, tx, secret).await) }
            });
            // shortcut: no connection cap or header timeout. The listener is tailnet-only and the secret is checked before the body is read. Add both if it is ever exposed.
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
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
        Ok(Self {
            listen: cfg.listen,
            server_url: cfg.server_url.trim_end_matches('/').to_string(),
            password: env(&cfg.password_env)?,
            secret: env(&cfg.secret_env)?,
            http: reqwest::Client::builder().timeout(SEND_TIMEOUT).build()?,
        })
    }
}

impl Channel for IMessage {
    fn name(&self) -> &str {
        "imessage"
    }

    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        let listener = TcpListener::bind(self.listen)
            .await
            .with_context(|| format!("binding {}", self.listen))?;
        tokio::select! {
            _ = serve(listener, tx.clone(), self.secret.clone()) => {}
            _ = tx.closed() => {}
        }
        anyhow::bail!("imessage adapter stopped")
    }

    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        let body = serde_json::json!({
            // One-to-one iMessage chat. BlueBubbles maps the `iMessage` prefix to `any` on macOS Tahoe.
            "chatGuid": format!("iMessage;-;{}", msg.recipient),
            "message": msg.text,
            "method": "apple-script",
            "tempGuid": msg.client_id,
        });
        let res = self
            .http
            .post(format!("{}/api/v1/message/text", self.server_url))
            .query(&[("password", &self.password)])
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                // The error text can carry the URL, and the URL carries the password. Keep the cause chain.
                let text = format!("{:#}", anyhow::Error::new(e.without_url()));
                let text = redact(&text, &self.password);
                anyhow::anyhow!("sending to BlueBubbles failed: {text}")
            })?;
        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            anyhow::bail!(
                "BlueBubbles rejected the send with {status}: {}",
                redact(&text, &self.password)
            );
        }
        Ok(())
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
        let got = to_inbound(NEW.as_bytes()).unwrap().unwrap();
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
        tokio::spawn(serve(listener, tx, secret.to_string()));
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

    /// Captures each request's path and query, and body, then answers with `reply`.
    async fn stub(status: u16, reply: &'static str) -> (String, mpsc::Receiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (cap_tx, cap_rx) = mpsc::channel(8);
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
                            let uri = req.uri().to_string();
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let _ = cap_tx
                                .send((uri, String::from_utf8_lossy(&body).into()))
                                .await;
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
            http: reqwest::Client::new(),
        }
    }

    fn outbound() -> Outbound {
        Outbound {
            channel: "imessage".into(),
            recipient: "+15551234567".into(),
            client_id: "client-1".into(),
            text: "hi there".into(),
        }
    }

    #[tokio::test]
    async fn send_posts_text_route_with_chat_guid_and_temp_guid() {
        let (base, mut cap) = stub(200, r#"{"status":200,"message":"Message sent!"}"#).await;
        adapter(base).send(&outbound()).await.unwrap();
        let (uri, body) = cap.recv().await.unwrap();
        assert_eq!(uri, "/api/v1/message/text?password=pw-hunter2");
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
}
