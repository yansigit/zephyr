//! A fake `agent.v1.AgentService/Run` endpoint for transport tests: plaintext HTTP/2 on
//! `127.0.0.1:0` plus a scripted server side.
//!
//! The fake shares nothing with `src/wire.rs` on purpose. It encodes the server messages it sends
//! and walks the client messages it receives with its own hand-written protobuf reader, so a wrong
//! field number, frame flag or header on the provider side fails a test instead of cancelling out
//! on both sides.
//!
//! Any test target can use it with `#[path = "support/fake_agent.rs"] mod fake_agent;`, point a
//! `SessionConfig::url` (or the real binary's `CURSOR_AGENT_URL`) at [`FakeAgent::url`], and end the
//! script with [`Step::EndStream`]. One script runs per accepted connection; [`FakeAgent::finish`]
//! reports the first connection's outcome, and every client-side violation of the script is an
//! error there rather than a silent pass.
#![allow(dead_code)] // shared by several test targets; not every helper is used by each of them.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use flate2::write::GzEncoder;
use flate2::Compression;
use h2::server::{handshake, SendResponse};
use h2::{RecvStream, SendStream};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::{timeout, Instant};

/// The path the provider posts to; asserted unless [`ExpectHeaders::path`] says otherwise.
pub const RUN_PATH: &str = "/agent.v1.AgentService/Run";
/// Connect frame flags.
const FLAG_PLAIN: u8 = 0x00;
const FLAG_GZIP: u8 = 0x01;
const FLAG_END_STREAM: u8 = 0x02;
/// How long the fake waits for a frame a script requires before failing.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `finish` waits for the script to report.
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the connection is kept open after the script so queued frames reach the client.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Window after `mcp_args` in which the client must stay quiet.
const QUIET_WINDOW: Duration = Duration::from_millis(400);

/// What the fake requires of the client's request headers. A field left `None` is not checked: the
/// bearer token of an external client (the real `rpi` binary) is minted at runtime, so a test
/// cannot predict it.
#[derive(Clone, Debug, Default)]
pub struct ExpectHeaders {
    pub path: Option<String>,
    /// Defaults to the address the fake bound.
    pub authority: Option<String>,
    pub token: Option<String>,
    pub request_id: Option<String>,
    pub conversation_id: Option<String>,
}

/// One scripted exchange. Steps run in order against the client's stream.
#[derive(Clone, Debug)]
pub enum Step {
    /// The client's first message: `AgentClientMessage{run_request}`.
    ExpectRunRequest,
    /// Ask for context and require the matching `exec_client_message{request_context_result}`.
    RequestContext {
        id: u32,
        exec_id: String,
    },
    /// `interaction_update{text_delta}`; `gzip` puts the frame on the wire compressed.
    TextDelta {
        text: String,
        gzip: bool,
    },
    ThinkingDelta {
        text: String,
    },
    TokenDelta {
        tokens: i64,
    },
    TurnEnded,
    /// Ask for a blob and require the client's `get_blob_result` to carry `expected`.
    KvGetBlob {
        blob_id: Vec<u8>,
        expected: Vec<u8>,
    },
    /// Ask for a tool call. The provider ends the turn here and lets the host run the tool, so the
    /// client must not answer.
    McpArgs {
        call_id: String,
        tool_name: String,
        args: Vec<(String, String)>,
    },
    /// Stay silent and require at least one client heartbeat in that window.
    ExpectHeartbeat {
        silence: Duration,
    },
    /// End the stream with a JSON trailer: `{}` for a clean end, `{"error": …}` for a failure.
    EndStream {
        trailer: String,
    },
}

impl Step {
    pub fn text(text: &str) -> Step {
        Step::TextDelta {
            text: text.to_string(),
            gzip: false,
        }
    }

    pub fn gzipped_text(text: &str) -> Step {
        Step::TextDelta {
            text: text.to_string(),
            gzip: true,
        }
    }

    pub fn request_context(id: u32, exec_id: &str) -> Step {
        Step::RequestContext {
            id,
            exec_id: exec_id.to_string(),
        }
    }

    pub fn clean_end() -> Step {
        Step::EndStream {
            trailer: "{}".to_string(),
        }
    }

    pub fn error_end(code: &str, message: &str) -> Step {
        Step::EndStream {
            trailer: format!(r#"{{"error":{{"code":"{code}","message":"{message}"}}}}"#),
        }
    }
}

/// A running fake endpoint. Drop it (or call [`FakeAgent::finish`]) when the exchange is done.
pub struct FakeAgent {
    /// Point the client at this: `SessionConfig::url` or `CURSOR_AGENT_URL`.
    pub url: String,
    done: oneshot::Receiver<Result<()>>,
}

impl FakeAgent {
    /// Serves `steps` against the first connection that arrives.
    pub async fn start(expect: ExpectHeaders, steps: Vec<Step>) -> FakeAgent {
        spawn_server(expect, Script::Steps(Arc::new(steps))).await
    }

    /// Answers with a non-200 status and `body` instead of opening a stream.
    pub async fn start_error(status: u16, body: &str) -> FakeAgent {
        spawn_server(
            ExpectHeaders::default(),
            Script::Error {
                status,
                body: body.to_string(),
            },
        )
        .await
    }

    /// Waits for the script to finish and panics if the client broke the protocol.
    pub async fn finish(self) {
        match timeout(FINISH_TIMEOUT, self.done).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => panic!("fake agent: {error:#}"),
            Ok(Err(_)) => panic!("fake agent: the script task ended without a result"),
            Err(_) => panic!("fake agent: the script did not finish within {FINISH_TIMEOUT:?}"),
        }
    }
}

#[derive(Clone)]
enum Script {
    Steps(Arc<Vec<Step>>),
    Error { status: u16, body: String },
}

async fn spawn_server(mut expect: ExpectHeaders, script: Script) -> FakeAgent {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake agent");
    let addr = listener.local_addr().expect("fake agent address");
    if expect.authority.is_none() {
        expect.authority = Some(addr.to_string());
    }
    let (done_tx, done) = oneshot::channel();
    let mut done_tx = Some(done_tx);
    tokio::spawn(async move {
        while let Ok((socket, _peer)) = listener.accept().await {
            let expect = expect.clone();
            let script = script.clone();
            let done = done_tx.take();
            tokio::spawn(async move {
                let result = serve(socket, expect, script).await;
                if let Some(done) = done {
                    let _ = done.send(result);
                }
            });
        }
    });
    FakeAgent {
        url: format!("http://{addr}{RUN_PATH}"),
        done,
    }
}

async fn serve(socket: TcpStream, expect: ExpectHeaders, script: Script) -> Result<()> {
    let mut connection = handshake(socket)
        .await
        .context("h2 handshake with the client")?;
    let (request, respond) = match connection.accept().await {
        Some(result) => result.context("client request")?,
        None => bail!("the client closed the connection before sending a request"),
    };
    let (parts, body) = request.into_parts();
    check_headers(&expect, &parts)?;

    // `h2` only moves bytes while the connection future is polled, so the script cannot simply
    // await the client's frames: it would starve the socket. Run it next to the connection.
    let mut script = Box::pin(run_script(script, respond, body));
    let mut drive = Box::pin(async { while connection.accept().await.is_some() {} });
    tokio::select! {
        result = &mut script => {
            // The client still has to read the frames queued above, so keep the connection alive
            // until it goes away instead of dropping the queued bytes with this task.
            let _ = timeout(DRAIN_TIMEOUT, &mut drive).await;
            result
        }
        () = &mut drive => match timeout(DRAIN_TIMEOUT, &mut script).await {
            Ok(result) => result,
            Err(_) => bail!("the client closed the connection while the script was still running"),
        },
    }
}

async fn run_script(
    script: Script,
    mut respond: SendResponse<Bytes>,
    mut body: RecvStream,
) -> Result<()> {
    match script {
        Script::Error { status, body: text } => {
            let response = http::Response::builder()
                .status(status)
                .header("content-type", "text/plain")
                .body(())
                .context("build the error response")?;
            let mut send = respond
                .send_response(response, false)
                .context("send the error response headers")?;
            send.send_data(Bytes::from(text), true)
                .context("send the error response body")?;
        }
        Script::Steps(steps) => {
            let response = http::Response::builder()
                .status(200)
                .header("content-type", "application/connect+proto")
                .body(())
                .context("build the response")?;
            let mut send = respond
                .send_response(response, false)
                .context("send the response headers")?;
            let mut pending = Vec::new();
            for step in steps.iter() {
                run_step(step, &mut body, &mut pending, &mut send).await?;
            }
        }
    }
    Ok(())
}

/// Asserts the Connect request headers the provider must send.
fn check_headers(expect: &ExpectHeaders, parts: &http::request::Parts) -> Result<()> {
    if parts.method != http::Method::POST {
        bail!("request method {} is not POST", parts.method);
    }
    let path = parts.uri.path();
    let want_path = expect.path.as_deref().unwrap_or(RUN_PATH);
    if path != want_path {
        bail!("request path {path:?} is not {want_path:?}");
    }
    if let Some(want) = &expect.authority {
        let got = parts
            .uri
            .authority()
            .map(|authority| authority.as_str().to_string());
        if got.as_deref() != Some(want.as_str()) {
            bail!("request :authority is {got:?}, expected {want:?}");
        }
    }
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
    };
    for (name, want) in [
        ("content-type", "application/connect+proto"),
        ("connect-protocol-version", "1"),
        ("connect-accept-encoding", "gzip"),
        ("te", "trailers"),
        ("user-agent", "connect-es/1.6.1"),
        ("x-ghost-mode", "true"),
        ("x-cursor-client-type", "cli"),
        (
            "x-cursor-agent-allowed-tools",
            "mcp_tool_call,get_mcp_tools_tool_call",
        ),
    ] {
        match header(name) {
            Some(got) if got == want => {}
            Some(got) => bail!("header {name} is {got:?}, expected {want:?}"),
            None => bail!("header {name} is missing"),
        }
    }
    let version = header("x-cursor-client-version")
        .ok_or_else(|| anyhow::anyhow!("header x-cursor-client-version is missing"))?;
    if !version.starts_with("cli-") {
        bail!("x-cursor-client-version {version:?} is not a cli build id");
    }
    // Never echo the authorization value itself: it is a credential on the real service.
    if let Some(token) = &expect.token {
        match header("authorization") {
            Some(got) if got == format!("Bearer {token}") => {}
            Some(_) => bail!("the authorization header does not carry the configured token"),
            None => bail!("the authorization header is missing"),
        }
    }
    for (name, want) in [
        ("x-request-id", &expect.request_id),
        ("x-session-id", &expect.conversation_id),
    ] {
        if let Some(want) = want {
            match header(name) {
                Some(got) if got == *want => {}
                Some(got) => bail!("header {name} is {got:?}, expected {want:?}"),
                None => bail!("header {name} is missing"),
            }
        }
    }
    Ok(())
}

async fn run_step(
    step: &Step,
    body: &mut RecvStream,
    pending: &mut Vec<u8>,
    send: &mut SendStream<Bytes>,
) -> Result<()> {
    match step {
        Step::ExpectRunRequest => {
            let message = next_client_message(body, pending).await?;
            if message.kind != ClientKind::RunRequest {
                bail!("expected the run request, got {}", message.kind);
            }
        }
        Step::RequestContext { id, exec_id } => {
            send_frame(send, FLAG_PLAIN, &request_context_args(*id, exec_id), false)?;
            let message = next_client_message(body, pending).await?;
            if message.kind != ClientKind::RequestContextResult {
                bail!("expected a request_context_result, got {}", message.kind);
            }
            if message.id != u64::from(*id) || message.exec_id != *exec_id {
                bail!(
                    "request_context_result echoed id={} exec_id={:?}, expected id={id} exec_id={exec_id:?}",
                    message.id,
                    message.exec_id
                );
            }
        }
        Step::TextDelta { text, gzip } => {
            let payload = text_delta(text);
            let (flag, payload) = if *gzip {
                (FLAG_GZIP, gzip_bytes(&payload))
            } else {
                (FLAG_PLAIN, payload)
            };
            send_frame(send, flag, &payload, false)?;
        }
        Step::ThinkingDelta { text } => {
            send_frame(send, FLAG_PLAIN, &thinking_delta(text), false)?;
        }
        Step::TokenDelta { tokens } => {
            send_frame(send, FLAG_PLAIN, &token_delta(*tokens), false)?;
        }
        Step::TurnEnded => {
            send_frame(send, FLAG_PLAIN, &turn_ended(), false)?;
        }
        Step::KvGetBlob { blob_id, expected } => {
            send_frame(send, FLAG_PLAIN, &get_blob_args(1, blob_id), false)?;
            let message = next_client_message(body, pending).await?;
            if message.kind != ClientKind::KvGetBlobResult {
                bail!("expected a get_blob_result, got {}", message.kind);
            }
            if message.blob != *expected {
                bail!(
                    "get_blob_result carried {:?}, expected {:?}",
                    String::from_utf8_lossy(&message.blob),
                    String::from_utf8_lossy(expected)
                );
            }
        }
        Step::McpArgs {
            call_id,
            tool_name,
            args,
        } => {
            send_frame(
                send,
                FLAG_PLAIN,
                &mcp_args(1, "exec-mcp", call_id, tool_name, args),
                false,
            )?;
            // The provider ends the turn at the tool call and lets the host execute it, so the
            // only frames allowed in the window after it are heartbeats.
            let deadline = Instant::now() + QUIET_WINDOW;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match timeout(remaining, next_client_frame(body, pending)).await {
                    Err(_) => break,
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(frame))) => {
                        let message = parse_client_message(&frame)?;
                        if message.kind != ClientKind::Heartbeat {
                            bail!(
                                "the client answered mcp_args with {}; the provider must end the turn instead",
                                message.kind
                            );
                        }
                    }
                    Ok(Err(error)) => return Err(error),
                }
            }
        }
        Step::ExpectHeartbeat { silence } => {
            let mut seen = 0;
            let deadline = Instant::now() + *silence;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match timeout(remaining, next_client_frame(body, pending)).await {
                    Err(_) => break,
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(frame))) => {
                        if parse_client_message(&frame)?.kind == ClientKind::Heartbeat {
                            seen += 1;
                        }
                    }
                    Ok(Err(error)) => return Err(error),
                }
            }
            if seen == 0 {
                bail!("the client sent no heartbeat during {silence:?} of server silence");
            }
        }
        Step::EndStream { trailer } => {
            send_frame(send, FLAG_END_STREAM, trailer.as_bytes(), true)?;
        }
    }
    Ok(())
}

fn send_frame(send: &mut SendStream<Bytes>, flag: u8, payload: &[u8], end: bool) -> Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.push(flag);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    send.send_data(Bytes::from(frame), end)
        .context("send a response frame")
}

/// The next request-body frame, or `None` once the client half-closes.
async fn next_client_frame(
    body: &mut RecvStream,
    pending: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    loop {
        if let Some(frame) = take_frame(pending)? {
            return Ok(Some(frame));
        }
        match timeout(FRAME_TIMEOUT, body.data()).await {
            Ok(Some(Ok(chunk))) => {
                let _ = body.flow_control().release_capacity(chunk.len());
                pending.extend_from_slice(&chunk);
            }
            Ok(Some(Err(error))) => bail!("the client's request stream failed: {error}"),
            Ok(None) => return Ok(None),
            Err(_) => bail!("timed out after {FRAME_TIMEOUT:?} waiting for a client frame"),
        }
    }
}

/// The next client message that is not a heartbeat (heartbeats may interleave with any reply).
async fn next_client_message(
    body: &mut RecvStream,
    pending: &mut Vec<u8>,
) -> Result<ClientMessage> {
    loop {
        match next_client_frame(body, pending).await? {
            Some(frame) => {
                let message = parse_client_message(&frame)?;
                if message.kind != ClientKind::Heartbeat {
                    return Ok(message);
                }
            }
            None => bail!("the client half-closed the stream while a reply was expected"),
        }
    }
}

fn take_frame(pending: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    if pending.len() < 5 {
        return Ok(None);
    }
    let flag = pending[0];
    let len = u32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]]) as usize;
    if pending.len() < 5 + len {
        return Ok(None);
    }
    let payload = pending[5..5 + len].to_vec();
    pending.drain(..5 + len);
    if flag & FLAG_GZIP != 0 {
        // The provider never compresses a request, so this is a protocol violation, not a case.
        bail!("the client sent a gzip-flagged request frame");
    }
    Ok(Some(payload))
}

fn gzip_bytes(payload: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload).expect("gzip a frame");
    encoder.finish().expect("finish a gzip frame")
}

// --------------------------------------------------------------------------
// Hand-written protobuf: server → client
// --------------------------------------------------------------------------

fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn field_varint(number: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(number << 3, &mut out);
    write_varint(value, &mut out);
    out
}

fn field_bytes(number: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint((number << 3) | 2, &mut out);
    write_varint(payload.len() as u64, &mut out);
    out.extend_from_slice(payload);
    out
}

fn field_str(number: u64, text: &str) -> Vec<u8> {
    field_bytes(number, text.as_bytes())
}

/// `AgentServerMessage{interaction_update{text_delta{text}}}`.
fn text_delta(text: &str) -> Vec<u8> {
    field_bytes(1, &field_bytes(1, &field_str(1, text)))
}

/// `AgentServerMessage{interaction_update{thinking_delta{text}}}`.
fn thinking_delta(text: &str) -> Vec<u8> {
    field_bytes(1, &field_bytes(4, &field_str(1, text)))
}

/// `AgentServerMessage{interaction_update{token_delta{tokens}}}`.
fn token_delta(tokens: i64) -> Vec<u8> {
    field_bytes(1, &field_bytes(8, &field_varint(1, tokens as u64)))
}

/// `AgentServerMessage{interaction_update{turn_ended{}}}`.
fn turn_ended() -> Vec<u8> {
    field_bytes(1, &field_bytes(14, &[]))
}

/// `AgentServerMessage{exec_server_message{id, exec_id, <member>}}`.
fn exec_server_message(id: u32, exec_id: &str, member: u64, payload: &[u8]) -> Vec<u8> {
    let mut exec = field_varint(1, u64::from(id));
    exec.extend(field_str(15, exec_id));
    exec.extend(field_bytes(member, payload));
    field_bytes(2, &exec)
}

/// `exec_server_message{request_context_args{}}`.
fn request_context_args(id: u32, exec_id: &str) -> Vec<u8> {
    exec_server_message(id, exec_id, 10, &[])
}

/// `exec_server_message{mcp_args{name, args, tool_call_id, provider_identifier, tool_name}}`.
fn mcp_args(
    id: u32,
    exec_id: &str,
    call_id: &str,
    tool_name: &str,
    args: &[(String, String)],
) -> Vec<u8> {
    let mut mcp = field_str(1, tool_name);
    for (key, value) in args {
        let mut entry = field_str(1, key);
        entry.extend(field_bytes(2, &field_str(3, value)));
        mcp.extend(field_bytes(2, &entry));
    }
    mcp.extend(field_str(3, call_id));
    mcp.extend(field_str(4, "zephyr"));
    mcp.extend(field_str(5, tool_name));
    exec_server_message(id, exec_id, 11, &mcp)
}

/// `AgentServerMessage{kv_server_message{id, get_blob_args{blob_id}}}`.
fn get_blob_args(id: u32, blob_id: &[u8]) -> Vec<u8> {
    let mut kv = field_varint(1, u64::from(id));
    kv.extend(field_bytes(2, &field_bytes(1, blob_id)));
    field_bytes(4, &kv)
}

/// `AgentClientMessage{exec_client_message{id, exec_id, request_context_result{}}}`: what a client
/// answers a `request_context_args` with. A test sends these bytes through `Session::send`.
pub fn request_context_result(id: u32, exec_id: &str) -> Vec<u8> {
    let mut exec = field_varint(1, u64::from(id));
    exec.extend(field_str(15, exec_id));
    exec.extend(field_bytes(10, &[]));
    field_bytes(2, &exec)
}

// --------------------------------------------------------------------------
// Hand-written protobuf: client → server
// --------------------------------------------------------------------------

/// One protobuf field: number, wire type, and the payload of a length-delimited field (`value`
/// carries a varint field).
pub struct Field<'a> {
    pub number: u64,
    pub wire: u8,
    pub value: u64,
    pub bytes: &'a [u8],
}

/// Walks protobuf fields without a schema.
pub fn fields(mut buf: &[u8]) -> Vec<Field<'_>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let Some((tag, rest)) = read_varint(buf) else {
            break;
        };
        match tag & 7 {
            0 => {
                let Some((value, rest)) = read_varint(rest) else {
                    break;
                };
                out.push(Field {
                    number: tag >> 3,
                    wire: 0,
                    value,
                    bytes: &[],
                });
                buf = rest;
            }
            2 => {
                let Some((len, rest)) = read_varint(rest) else {
                    break;
                };
                let len = len as usize;
                if rest.len() < len {
                    break;
                }
                out.push(Field {
                    number: tag >> 3,
                    wire: 2,
                    value: 0,
                    bytes: &rest[..len],
                });
                buf = &rest[len..];
            }
            _ => break, // the provider sends no fixed-width fields
        }
    }
    out
}

pub fn find<'a, 'p>(fields: &'a [Field<'p>], number: u64) -> Option<&'a Field<'p>> {
    fields.iter().find(|field| field.number == number)
}

fn read_varint(mut buf: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for shift in 0..10 {
        let (&byte, rest) = buf.split_first()?;
        value |= u64::from(byte & 0x7f) << (shift * 7);
        buf = rest;
        if byte & 0x80 == 0 {
            return Some((value, buf));
        }
    }
    None
}

/// What the client sent, classified by the `AgentClientMessage` oneof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientKind {
    RunRequest,
    RequestContextResult,
    McpResult,
    KvGetBlobResult,
    KvSetBlobAck,
    Heartbeat,
    Other,
}

impl std::fmt::Display for ClientKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ClientKind::RunRequest => "run_request",
            ClientKind::RequestContextResult => "request_context_result",
            ClientKind::McpResult => "mcp_result",
            ClientKind::KvGetBlobResult => "get_blob_result",
            ClientKind::KvSetBlobAck => "set_blob_result",
            ClientKind::Heartbeat => "client_heartbeat",
            ClientKind::Other => "an unrecognised message",
        };
        f.write_str(name)
    }
}

pub struct ClientMessage {
    pub kind: ClientKind,
    pub id: u64,
    pub exec_id: String,
    pub blob: Vec<u8>,
}

fn parse_client_message(payload: &[u8]) -> Result<ClientMessage> {
    let mut message = ClientMessage {
        kind: ClientKind::Other,
        id: 0,
        exec_id: String::new(),
        blob: Vec::new(),
    };
    let top = fields(payload);
    if find(&top, 1).is_some() {
        message.kind = ClientKind::RunRequest;
        return Ok(message);
    }
    if find(&top, 7).is_some() {
        message.kind = ClientKind::Heartbeat;
        return Ok(message);
    }
    if let Some(exec) = find(&top, 2) {
        let exec = fields(exec.bytes);
        message.id = find(&exec, 1).map(|field| field.value).unwrap_or(0);
        message.exec_id = find(&exec, 15)
            .and_then(|field| std::str::from_utf8(field.bytes).ok())
            .unwrap_or_default()
            .to_string();
        message.kind = if find(&exec, 10).is_some() {
            ClientKind::RequestContextResult
        } else if find(&exec, 11).is_some() {
            ClientKind::McpResult
        } else {
            ClientKind::Other
        };
        return Ok(message);
    }
    if let Some(kv) = find(&top, 3) {
        let kv = fields(kv.bytes);
        message.id = find(&kv, 1).map(|field| field.value).unwrap_or(0);
        if let Some(result) = find(&kv, 2) {
            message.kind = ClientKind::KvGetBlobResult;
            message.blob = find(&fields(result.bytes), 1)
                .map(|field| field.bytes.to_vec())
                .unwrap_or_default();
        } else if find(&kv, 3).is_some() {
            message.kind = ClientKind::KvSetBlobAck;
        }
        return Ok(message);
    }
    Ok(message)
}
