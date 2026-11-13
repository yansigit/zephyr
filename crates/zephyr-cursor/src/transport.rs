//! The bi-directional Connect stream to Cursor's agent service, and the blob store its history is
//! made of.
//!
//! The request body and the response body are live at the same time: `h2` sends DATA frames while
//! the server's DATA frames are still arriving, which is what lets the provider answer a KV
//! `getBlob` or a `requestContext` handshake mid-turn. `Session::next` therefore never blocks
//! forever: it heartbeats every 5 s of silence, and it owns framing, gunzip and decode so callers
//! only see [`wire::ServerMsg`].
use crate::wire;
use anyhow::{bail, Context};
use bytes::Bytes;
use flate2::read::GzDecoder;
use http::{HeaderValue, Method, Request};
use sha2::Digest;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// History entries the server asks for by id, keyed by SHA-256 of the entry's JSON bytes.
#[derive(Clone, Default)]
pub struct BlobStore(Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>);

impl BlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `entry` and returns its id.
    pub fn insert(&self, entry: Vec<u8>) -> Vec<u8> {
        let id = sha2::Sha256::digest(&entry).to_vec();
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), entry);
        id
    }

    /// Ids for every entry the server wrote back (`setBlob`), so a turn's summary/pinned blobs are
    /// readable within the same conversation.
    pub fn put(&self, id: Vec<u8>, data: Vec<u8>) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, data);
    }

    pub fn get(&self, id: &[u8]) -> Option<Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }
}

/// Everything a session needs to open: where, as whom, and under which ids.
#[derive(Clone)]
pub struct SessionConfig {
    /// Full `https://host/agent.v1.AgentService/Run` URL, or `http://` for a plaintext test server.
    pub url: String,
    pub token: String,
    /// One per conversation: Cursor keys its server-side state (and its billing) by this.
    pub conversation_id: String,
    pub request_id: String,
}

// Hand-written so a `{:?}` in a log line never prints the bearer token.
impl std::fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConfig")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("conversation_id", &self.conversation_id)
            .field("request_id", &self.request_id)
            .finish()
    }
}

/// A live `Run` call.
pub struct Session {
    /// Outbound `AgentClientMessage` payloads, framed and written by the sender task. Dropping it
    /// is what half-closes the request body.
    tx: mpsc::Sender<Vec<u8>>,
    /// The response body, kept open for the whole turn.
    body: h2::RecvStream,
    /// Response bytes received but not yet consumed as whole Connect frames.
    pending: Vec<u8>,
    /// Set once the server's end-of-stream trailer (or its half-close) has been seen.
    finished: bool,
    /// Silence budget before [`Session::next`] writes a heartbeat.
    heartbeat: Duration,
    sender_task: JoinHandle<()>,
    connection_task: JoinHandle<()>,
}

impl Session {
    /// Opens the stream with the Connect headers and `run_request` as its first message. That
    /// message goes out before the response headers are awaited: a Connect server may hold its
    /// headers until it has read the request. Fails on TLS/HTTP2/status errors.
    pub async fn connect(config: &SessionConfig, run_request: Vec<u8>) -> anyhow::Result<Session> {
        let target = Target::parse(&config.url)?;
        let heartbeat = heartbeat_interval();

        // Under the network floor the agent's processes reach nothing but the proxy (§4.20).
        let proxy = proxy_for(&target, |k| std::env::var(k).ok())?;
        let (host, port) = proxy
            .as_ref()
            .map_or((target.host.as_str(), target.port), |p| {
                (p.host.as_str(), p.port)
            });
        let mut tcp = timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((host, port)),
        )
        .await
        .with_context(|| format!("timed out connecting to {host}:{port}"))?
        .with_context(|| format!("failed to connect to {host}:{port}"))?;
        tcp.set_nodelay(true).ok();
        if proxy.is_some() {
            timeout(CONNECT_TIMEOUT, tunnel(&mut tcp, &target))
                .await
                .context("timed out waiting for the proxy's CONNECT answer")??;
        }

        // TLS and plaintext h2 differ only in the byte stream `handshake` runs on; the spawned
        // connection task owns whichever one we got.
        let (h2, connection_task) = if target.tls {
            let connector = tokio_rustls::TlsConnector::from(tls_config());
            let server_name =
                tokio_rustls::rustls::pki_types::ServerName::try_from(target.host.clone())
                    .context("agent URL host is not a valid TLS server name")?;
            let tls = timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
                .await
                .context("timed out during the TLS handshake with the agent service")?
                .context("TLS handshake with the agent service failed")?;
            if tls.get_ref().1.alpn_protocol() != Some(b"h2") {
                bail!("agent service did not negotiate the h2 ALPN protocol");
            }
            let (h2, connection) = timeout(CONNECT_TIMEOUT, h2::client::handshake(tls))
                .await
                .context("timed out during the HTTP/2 handshake with the agent service")?
                .context("HTTP/2 handshake with the agent service failed")?;
            (
                h2,
                tokio::spawn(async move {
                    let _ = connection.await;
                }),
            )
        } else {
            let (h2, connection) = timeout(CONNECT_TIMEOUT, h2::client::handshake(tcp))
                .await
                .context("timed out during the HTTP/2 handshake with the agent service")?
                .context("HTTP/2 handshake with the agent service failed")?;
            (
                h2,
                tokio::spawn(async move {
                    let _ = connection.await;
                }),
            )
        };

        let mut h2 = match timeout(CONNECT_TIMEOUT, h2.ready()).await {
            Ok(Ok(h2)) => h2,
            Ok(Err(error)) => {
                connection_task.abort();
                return Err(error)
                    .context("the HTTP/2 connection to the agent service is not ready");
            }
            Err(_) => {
                connection_task.abort();
                bail!("timed out waiting for the HTTP/2 connection to the agent service");
            }
        };

        let request = build_request(&target, config)?;
        let (response, send_stream) = h2
            .send_request(request, false)
            .context("failed to send the agent request headers")?;

        // The request body stays open until the caller drops its sender: the server asks for
        // blobs and context on this same stream, and `Session::send` answers them.
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(OUTBOUND_QUEUE);
        let sender_task = tokio::spawn(async move {
            let mut send_stream = send_stream;
            while let Some(payload) = rx.recv().await {
                if send_stream
                    .send_data(Bytes::from(wire::connect_frame(&payload)), false)
                    .is_err()
                {
                    // The connection or the stream is gone; dropping `rx` makes `send` fail.
                    return;
                }
            }
            let _ = send_stream.send_data(Bytes::new(), true);
        });
        // Cannot fail: the queue is empty and the sender task holds the receiver.
        let _ = tx.send(run_request).await;

        let response = match timeout(CONNECT_TIMEOUT, response).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                sender_task.abort();
                connection_task.abort();
                return Err(error).context("the agent request failed before the response headers");
            }
            Err(_) => {
                sender_task.abort();
                connection_task.abort();
                bail!("timed out waiting for the agent service response headers");
            }
        };

        let status = response.status();
        let body = response.into_body();
        if status != http::StatusCode::OK {
            let detail = error_body(body).await;
            sender_task.abort();
            connection_task.abort();
            bail!("agent service returned HTTP {status}: {detail}");
        }

        Ok(Session {
            tx,
            body,
            pending: Vec::new(),
            finished: false,
            heartbeat,
            sender_task,
            connection_task,
        })
    }

    /// Writes one `AgentClientMessage` as a Connect frame.
    pub async fn send(&self, message: Vec<u8>) -> anyhow::Result<()> {
        self.tx
            .send(message)
            .await
            .map_err(|_| anyhow::anyhow!("the agent stream is closed"))
    }

    /// The next decoded server message, or `None` once the server half-closes. Sends a heartbeat
    /// frame after [`HEARTBEAT_INTERVAL`] of silence.
    pub async fn next(&mut self) -> anyhow::Result<Option<wire::ServerMsg>> {
        match self.next_payload().await? {
            Some(payload) => Ok(Some(wire::decode_server_message(&payload))),
            None => Ok(None),
        }
    }

    /// The next decompressed response message, or `None` at the end of the stream. [`Session::next`]
    /// decodes this; keeping the split lets the framing, gunzip, trailer and heartbeat paths be
    /// exercised without depending on the protobuf decoder.
    async fn next_payload(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            if self.finished {
                return Ok(None);
            }
            match next_frame(&mut self.pending)? {
                Some(Frame::Payload(payload)) => return Ok(Some(payload)),
                Some(Frame::EndStream(trailer)) => {
                    self.finished = true;
                    if let Some(error) = trailer_error(&trailer) {
                        return Err(error);
                    }
                    return Ok(None);
                }
                None => {}
            }

            // No whole frame buffered: read more, heartbeating if the server stays quiet.
            match timeout(self.heartbeat, self.body.data()).await {
                Ok(Some(Ok(chunk))) => {
                    let _ = self.body.flow_control().release_capacity(chunk.len());
                    self.pending.extend_from_slice(&chunk);
                }
                Ok(Some(Err(error))) => {
                    return Err(error).context("the agent response stream failed");
                }
                Ok(None) => {
                    self.finished = true;
                    if self.pending.is_empty() {
                        return Ok(None);
                    }
                    let buffered = self.pending.len();
                    self.pending.clear();
                    bail!("agent response ended after {buffered} bytes of an incomplete Connect frame");
                }
                Err(_) => self.heartbeat().await?,
            }
        }
    }

    /// Writes one client heartbeat. A closed channel means the stream is already gone, so the
    /// caller must stop waiting for a response that can no longer arrive.
    async fn heartbeat(&mut self) -> anyhow::Result<()> {
        if self.tx.send(wire::encode_client_heartbeat()).await.is_err() {
            bail!("the agent stream closed while waiting for a response");
        }
        Ok(())
    }

    /// Half-closes the request body and lets the connection task finish.
    pub async fn close(self) {
        // The sender task sees the closed channel and writes an empty DATA frame with END_STREAM.
        drop(self.tx);
        let _ = timeout(CLOSE_GRACE, self.sender_task).await;
        let _ = timeout(CLOSE_GRACE, self.connection_task).await;
    }
}

/// Silence after which the client heartbeats, so a long thinking step is not reaped by a proxy.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// Connection + response-headers deadline.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// `x-cursor-agent-allowed-tools`: `ToolCall` oneof names, comma-joined.
pub const ALLOWED_TOOLS: &str = "mcp_tool_call,get_mcp_tools_tool_call";
/// Outbound frames buffered before `Session::send` applies backpressure.
const OUTBOUND_QUEUE: usize = 32;
/// How long `close` waits for the half-close and the connection to flush before detaching them.
const CLOSE_GRACE: Duration = Duration::from_millis(250);
/// Connect frame flags (`connect-es` streaming protocol).
const COMPRESSED_FLAG: u8 = 0x01;
const END_STREAM_FLAG: u8 = 0x02;
/// Bytes of a non-200 response body echoed in the error.
const ERROR_BODY_LIMIT: usize = 512;

/// Where to connect and what to ask for, split out of [`SessionConfig::url`].
pub(crate) struct Target {
    pub(crate) tls: bool,
    /// Bare host, without brackets or port: used for DNS, TLS SNI and `:authority`.
    pub(crate) host: String,
    port: u16,
    /// `host[:port]` exactly as the URL spelled it; the caller's port, if any, is kept.
    authority: String,
    path: String,
}

impl Target {
    pub(crate) fn parse(url: &str) -> anyhow::Result<Target> {
        let (scheme, rest) = url
            .split_once("://")
            .with_context(|| format!("agent URL {url:?} has no scheme"))?;
        let tls = match scheme {
            "https" => true,
            "http" => false,
            other => bail!("agent URL scheme {other:?} is not supported (use https or http)"),
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let authority = authority.trim();
        if authority.is_empty() {
            bail!("agent URL {url:?} has no host");
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                let port = port
                    .parse::<u16>()
                    .with_context(|| format!("agent URL {url:?} has an invalid port"))?;
                (host, port)
            }
            _ => (authority, if tls { 443 } else { 80 }),
        };
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        if host.is_empty() {
            bail!("agent URL {url:?} has no host");
        }
        Ok(Target {
            tls,
            host,
            port,
            authority: authority.to_string(),
            path: path.to_string(),
        })
    }

    fn scheme(&self) -> &'static str {
        if self.tls {
            "https"
        } else {
            "http"
        }
    }
}

/// The `POST` that opens the stream: Connect headers, no body yet (`h2` sends DATA frames as
/// `Session::send` hands them over).
fn build_request(target: &Target, config: &SessionConfig) -> anyhow::Result<Request<()>> {
    // The token only ever lives in this header value: never logged, never in an error message.
    let authorization = HeaderValue::from_str(&format!("Bearer {}", config.token))
        .context("the access token contains characters an HTTP header cannot carry")?;
    let request_id = header_value(&config.request_id, "x-request-id")?;
    let session_id = header_value(&config.conversation_id, "x-session-id")?;
    Request::builder()
        .method(Method::POST)
        .uri(format!(
            "{}://{}{}",
            target.scheme(),
            target.authority,
            target.path
        ))
        .header("authorization", authorization)
        .header("content-type", "application/connect+proto")
        .header("connect-protocol-version", "1")
        .header("connect-accept-encoding", "gzip")
        .header("te", "trailers")
        .header("user-agent", "connect-es/1.6.1")
        .header("x-ghost-mode", "true")
        .header("x-cursor-client-type", "cli")
        .header("x-cursor-client-version", wire::CLIENT_VERSION)
        .header("x-request-id", request_id)
        // Removes Cursor's built-in tool registry: without it the model calls Cursor's own tools,
        // which this process cannot execute. The MCP catalog lookup is required and Cursor serves
        // it itself (live: "Required tool GET_MCP_TOOLS not found"; value from oh-my-pi).
        .header("x-cursor-agent-allowed-tools", ALLOWED_TOOLS)
        .header("x-session-id", session_id)
        .body(())
        .context("failed to build the agent request")
}

fn header_value(raw: &str, name: &str) -> anyhow::Result<HeaderValue> {
    HeaderValue::from_str(raw).with_context(|| format!("{name} is not a valid HTTP header value"))
}

/// The most of a proxy's answer to `CONNECT` that is read.
const PROXY_HEAD_LIMIT: usize = 8 * 1024;

/// The proxy the environment names for `target` (`https_proxy`/`HTTPS_PROXY` for https,
/// `http_proxy`/`HTTP_PROXY` for http), unless `no_proxy`/`NO_PROXY` names its host or a
/// domain it is in.
// shortcut: no proxy credentials, https:// proxies, ports or CIDRs in NO_PROXY; zephyr's own
// proxy needs none of them.
fn proxy_for(
    target: &Target,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Option<Target>> {
    let var = |names: [&str; 2]| {
        names
            .into_iter()
            .find_map(|n| env(n).filter(|v| !v.trim().is_empty()))
    };
    let names = if target.tls {
        ["https_proxy", "HTTPS_PROXY"]
    } else {
        ["http_proxy", "HTTP_PROXY"]
    };
    let Some(url) = var(names) else {
        return Ok(None);
    };
    let host = target.host.to_ascii_lowercase();
    let bypass = var(["no_proxy", "NO_PROXY"])
        .unwrap_or_default()
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .any(|e| e == "*" || (!e.is_empty() && (host == e || host.ends_with(&format!(".{e}")))));
    if bypass {
        return Ok(None);
    }
    let url = url.trim();
    let url = if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}")
    };
    // The variable's value is never echoed: it may carry a password.
    match Target::parse(&url) {
        Ok(proxy) if !proxy.tls && !proxy.authority.contains('@') => Ok(Some(proxy)),
        _ => bail!("the proxy variable is not an http://host:port URL"),
    }
}

/// Asks the proxy for a tunnel to `target` and reads its answer up to the blank line, a byte at
/// a time so nothing the server sends after it is taken from the stream TLS or h2 reads next.
async fn tunnel(tcp: &mut tokio::net::TcpStream, target: &Target) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    };
    tcp.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await
        .context("failed to write to the proxy")?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= PROXY_HEAD_LIMIT {
            bail!("the proxy's answer to CONNECT is over {PROXY_HEAD_LIMIT} bytes");
        }
        let byte = tcp
            .read_u8()
            .await
            .context("the proxy closed before answering CONNECT")?;
        head.push(byte);
    }
    let head = String::from_utf8_lossy(&head);
    let line = head.lines().next().unwrap_or_default();
    if line.split_whitespace().nth(1) != Some("200") {
        bail!("the proxy refused CONNECT {authority}: {line}");
    }
    Ok(())
}

/// webpki roots + `h2` ALPN, matching what the `cursor-agent` CLI presents.
// Adapted from 1jehuang/jcode (MIT), agent_transport.rs
fn tls_config() -> Arc<tokio_rustls::rustls::ClientConfig> {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(config)
}

/// The heartbeat budget for a connection, read once per [`Session::connect`].
///
/// `CURSOR_HEARTBEAT_MS` overrides [`HEARTBEAT_INTERVAL`] so a test can observe a heartbeat
/// without waiting five seconds; production keeps the 5 s default.
fn heartbeat_interval() -> Duration {
    std::env::var("CURSOR_HEARTBEAT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(HEARTBEAT_INTERVAL)
}

/// One response frame: a message payload, or the JSON end-of-stream trailer.
enum Frame {
    Payload(Vec<u8>),
    EndStream(Vec<u8>),
}

/// Largest compressed or decompressed frame payload accepted off the wire.
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Pops one complete `[flag][len:u32 be][payload]` frame off `pending`, gunzipping a compressed
/// payload. `Ok(None)` means the buffer holds only part of a frame.
// Adapted from 1jehuang/jcode (MIT), agent_transport.rs
fn next_frame(pending: &mut Vec<u8>) -> anyhow::Result<Option<Frame>> {
    if pending.len() < 5 {
        return Ok(None);
    }
    let flag = pending[0];
    let len = u32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]]) as usize;
    if len > MAX_FRAME {
        bail!("a response frame of {len} bytes exceeds the 16 MiB limit");
    }
    let end = 5 + len;
    if pending.len() < end {
        return Ok(None);
    }
    let mut payload = pending[5..end].to_vec();
    pending.drain(..end);
    if flag & COMPRESSED_FLAG != 0 {
        let mut decoded = Vec::new();
        GzDecoder::new(&payload[..])
            .take(MAX_FRAME as u64 + 1)
            .read_to_end(&mut decoded)
            .context("a gzip-flagged response frame is not valid gzip")?;
        if decoded.len() > MAX_FRAME {
            bail!("a gzip response frame decompresses past the 16 MiB limit");
        }
        payload = decoded;
    }
    Ok(Some(if flag & END_STREAM_FLAG != 0 {
        Frame::EndStream(payload)
    } else {
        Frame::Payload(payload)
    }))
}

/// An end-of-stream trailer is a JSON object: `{}` ends a healthy stream, `{"error": {"code": …,
/// "message": …}}` ends a failed one.
fn trailer_error(trailer: &[u8]) -> Option<anyhow::Error> {
    let json: serde_json::Value = serde_json::from_slice(trailer).ok()?;
    let error = json.get("error")?;
    if error.is_null() {
        return None;
    }
    let code = error
        .get("code")
        .map(|code| {
            code.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| code.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let message = error
        .get("message")
        .and_then(|message| message.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    Some(anyhow::anyhow!(
        "agent stream failed with code {code}: {message}"
    ))
}

/// Up to [`ERROR_BODY_LIMIT`] bytes of a non-200 response, for the error message.
async fn error_body(mut body: h2::RecvStream) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    while bytes.len() < ERROR_BODY_LIMIT {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match timeout(remaining, body.data()).await {
            Ok(Some(Ok(chunk))) => {
                let _ = body.flow_control().release_capacity(chunk.len());
                bytes.extend_from_slice(&chunk);
            }
            _ => break,
        }
    }
    bytes.truncate(ERROR_BODY_LIMIT);
    String::from_utf8_lossy(&bytes).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::{self, Read};

    #[test]
    fn session_config_debug_redacts_token() {
        let config = SessionConfig {
            url: "https://agent.example/Run".into(),
            token: "secret-token-value".into(),
            conversation_id: "conv-1".into(),
            request_id: "req-1".into(),
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("secret-token-value"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn the_proxy_is_the_one_for_the_scheme_unless_no_proxy_names_the_host() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let at = |url: &str, vars| {
            let target = Target::parse(url).unwrap();
            proxy_for(&target, env(vars))
                .unwrap()
                .map(|p| format!("{}:{}", p.host, p.port))
        };
        let floor: &[(&str, &str)] = &[
            ("HTTPS_PROXY", "http://127.0.0.1:7792"),
            ("NO_PROXY", "localhost,127.0.0.1,::1"),
        ];
        let cursor = "https://agentn.global.api5.cursor.sh/agent.v1.AgentService/Run";
        assert_eq!(at(cursor, floor).as_deref(), Some("127.0.0.1:7792"));
        assert_eq!(at("https://127.0.0.1:9/x", floor), None);
        assert_eq!(at("https://[::1]:9/x", floor), None);
        assert_eq!(at("http://agent.example/x", floor), None, "no http_proxy");
        assert_eq!(at(cursor, &[]), None);
        let skip: &[(&str, &str)] = &[("https_proxy", "127.0.0.1:1"), ("no_proxy", ".cursor.sh")];
        assert_eq!(at(cursor, skip), None);
        assert_eq!(
            at("https://notcursor.sh/x", skip).as_deref(),
            Some("127.0.0.1:1")
        );
        let secret: &[(&str, &str)] = &[("HTTPS_PROXY", "http://u:hunter2@proxy:8080")];
        let e = proxy_for(&Target::parse(cursor).unwrap(), env(secret))
            .err()
            .expect("a proxy URL with credentials is refused");
        assert!(!e.to_string().contains("hunter2"), "{e}");
    }

    #[test]
    fn oversize_frame_length_is_rejected_before_the_body_arrives() {
        let mut pending = vec![0];
        pending.extend_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        let err = next_frame(&mut pending)
            .err()
            .expect("an oversize frame must be rejected with only its header buffered");
        assert!(
            err.to_string().contains("exceeds the 16 MiB limit"),
            "{err}"
        );
    }

    #[test]
    fn gzip_frame_that_inflates_past_the_cap_is_rejected() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        io::copy(&mut io::repeat(0).take(MAX_FRAME as u64 + 1), &mut encoder).unwrap();
        let gz = encoder.finish().unwrap();
        let mut pending = vec![COMPRESSED_FLAG];
        pending.extend_from_slice(&(gz.len() as u32).to_be_bytes());
        pending.extend_from_slice(&gz);
        let err = next_frame(&mut pending)
            .err()
            .expect("a gzip bomb past the cap must be rejected");
        assert!(
            err.to_string()
                .contains("decompresses past the 16 MiB limit"),
            "{err}"
        );
    }
}
