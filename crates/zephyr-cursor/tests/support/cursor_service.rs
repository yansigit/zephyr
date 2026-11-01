//! A stand-in for Cursor's `agent.v1.AgentService/Run` stream, written against the real protobuf
//! layout rather than against this crate's encoders: the point of the end-to-end test is to catch a
//! field number or framing mistake in `src/wire.rs`, so the far end cannot share its code.
//!
//! It speaks plain HTTP/2 with prior knowledge on loopback (the client allows that, and TLS is not
//! worth standing up in a test). One `Run` request is one turn: the script below is keyed off how
//! many requests have arrived, and every request is checked against what the previous turn's answer
//! should have produced.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use flate2::read::GzDecoder;
use h2::server;
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// What the fake service saw, checked by the test after rpi exits.
#[derive(Debug, Default, Clone)]
pub struct Observed {
    pub requests: usize,
    /// `(model_id, tool names, blob count, action text or None for resume)` per `Run`.
    pub runs: Vec<RunShape>,
    /// Blob JSON, in the order the client served them (`GetBlobResult`).
    pub blobs: Vec<String>,
    /// How many framing replies the client sent back (`requestContext`, KV, rejections).
    pub replies: usize,
    /// Tool names from the client's `RequestContext` reply.
    pub context_tools: Vec<String>,
    pub headers: BTreeMap<String, String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RunShape {
    pub model: String,
    pub tools: Vec<String>,
    pub blob_ids: usize,
    pub action: Option<String>,
    pub conversation_id: String,
}

pub struct Service {
    pub port: u16,
    pub observed: Arc<Mutex<Observed>>,
}

/// The file the model is told to create. Passed in so the test owns the assertion.
pub struct Script {
    pub file: String,
}

pub async fn start(script: Script) -> std::io::Result<Service> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let observed = Arc::new(Mutex::new(Observed::default()));
    let state = Arc::new(Mutex::new(script));
    let observer = Arc::clone(&observed);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&observer);
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                if let Err(error) = serve(socket, Arc::clone(&seen), state).await {
                    // Client-side closes are normal; only note real failures.
                    let text = error.to_string();
                    if !text.contains("connection closed") && !text.contains("reset") {
                        seen.lock().unwrap().errors.push(text);
                    }
                }
            });
        }
    });
    Ok(Service { port, observed })
}

async fn serve(
    socket: tokio::net::TcpStream,
    observed: Arc<Mutex<Observed>>,
    script: Arc<Mutex<Script>>,
) -> anyhow::Result<()> {
    let mut connection = server::handshake(socket).await?;
    // `h2` only moves bytes while `accept` is polled, so each stream runs on its own task.
    while let Some(accepted) = connection.accept().await {
        let (request, respond) = accepted?;
        let (observed, script) = (Arc::clone(&observed), Arc::clone(&script));
        tokio::spawn(async move {
            if let Err(error) = stream(request, respond, Arc::clone(&observed), script).await {
                // The client cancels a stream it has finished with; only note real failures.
                let text = error.to_string();
                if !text.contains("connection closed") && !text.contains("reset") {
                    observed.lock().unwrap().errors.push(text);
                }
            }
        });
    }
    Ok(())
}

async fn stream(
    request: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    observed: Arc<Mutex<Observed>>,
    script: Arc<Mutex<Script>>,
) -> anyhow::Result<()> {
    if observed.lock().unwrap().headers.is_empty() {
        let mut headers = BTreeMap::new();
        for (name, value) in request.headers().iter() {
            headers.insert(
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            );
        }
        observed.lock().unwrap().headers = headers;
    }
    let send = respond.send_response(
        http::Response::builder()
            .status(200)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .body(())
            .unwrap(),
        false,
    )?;
    let mut body = request.into_body();
    let mut buffer: Vec<u8> = Vec::new();
    let mut turn = Turn::new(send, Arc::clone(&observed), Arc::clone(&script));
    loop {
        let Some(chunk) = body.data().await else {
            break;
        };
        let chunk = chunk?;
        let _ = body.flow_control().release_capacity(chunk.len());
        buffer.extend_from_slice(&chunk);
        // Frames are `[flags][len u32 be][payload]`; a payload can be split across DATA frames.
        while buffer.len() >= 5 {
            let flags = buffer[0];
            let len = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
            if buffer.len() < 5 + len {
                break;
            }
            let mut payload = buffer[5..5 + len].to_vec();
            buffer.drain(..5 + len);
            if flags & 0x01 != 0 {
                let mut decoded = Vec::new();
                std::io::Read::read_to_end(&mut GzDecoder::new(&payload[..]), &mut decoded)?;
                payload = decoded;
            }
            if flags & 0x02 != 0 {
                continue; // end-of-stream trailer
            }
            turn.frame(&payload).await?;
        }
    }
    turn.finish().await?;
    Ok(())
}

struct Turn {
    send: h2::SendStream<Bytes>,
    observed: Arc<Mutex<Observed>>,
    script: Arc<Mutex<Script>>,
    /// Set once the request frame for this stream has been answered.
    answered: bool,
}

impl Turn {
    fn new(
        send: h2::SendStream<Bytes>,
        observed: Arc<Mutex<Observed>>,
        script: Arc<Mutex<Script>>,
    ) -> Self {
        Self {
            send,
            observed,
            script,
            answered: false,
        }
    }

    async fn frame(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        let Some((number, wire, body)) = first_field(payload) else {
            return Ok(());
        };
        match (number, wire) {
            // AgentClientMessage.run_request
            (1, 2) => self.run_request(body).await?,
            // AgentClientMessage.exec_client_message
            (2, 2) => {
                let tools = field(body, 10, 2)
                    .and_then(|result| field(result, 1, 2))
                    .and_then(|success| field(success, 1, 2))
                    .map(|context| {
                        repeated(context, 7, 2)
                            .filter_map(|tool| text(tool, 1))
                            .collect::<Vec<_>>()
                    });
                if let Some(tools) = &tools {
                    // The reply must carry back the tools the run request advertised, or the model
                    // would be offered tools the client cannot run.
                    let wanted = self
                        .observed
                        .lock()
                        .unwrap()
                        .runs
                        .last()
                        .map(|run| run.tools.clone())
                        .unwrap_or_default();
                    if tools != &wanted {
                        self.observed
                            .lock()
                            .unwrap()
                            .errors
                            .push(format!("context tools {tools:?} != {wanted:?}"));
                    }
                }
                let mut observed = self.observed.lock().unwrap();
                observed.replies += 1;
                if let Some(tools) = tools {
                    observed.context_tools = tools;
                }
                if !has_field(body, 10, 2) && !has_field(body, 11, 2) {
                    observed
                        .errors
                        .push("exec client message without a result".into());
                }
            }
            // AgentClientMessage.kv_client_message: GetBlobResult.blob_data
            (3, 2) => {
                let data = field(body, 2, 2).and_then(|result| field(result, 1, 2));
                let mut observed = self.observed.lock().unwrap();
                observed.replies += 1;
                match data {
                    Some(blob) => observed
                        .blobs
                        .push(String::from_utf8_lossy(blob).into_owned()),
                    None => observed.errors.push("blob reply without blob_data".into()),
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn run_request(&mut self, body: &[u8]) -> anyhow::Result<()> {
        let state = field(body, 1, 2).unwrap_or_default();
        let action = field(body, 2, 2).unwrap_or_default();
        let model_details = field(body, 3, 2).unwrap_or_default();
        let tools = field(body, 4, 2).unwrap_or_default();
        let shape = RunShape {
            model: text(model_details, 1).unwrap_or_default(),
            tools: repeated(tools, 1, 2)
                .filter_map(|definition| text(definition, 1))
                .collect(),
            blob_ids: repeated(state, 1, 2).filter(|id| !id.is_empty()).count(),
            action: field(action, 1, 2)
                .and_then(|user| field(user, 1, 2))
                .and_then(|message| text(message, 1)),
            conversation_id: text(body, 5).unwrap_or_default(),
        };
        let turn_index = {
            let mut observed = self.observed.lock().unwrap();
            observed.requests += 1;
            observed.runs.push(shape.clone());
            observed.requests - 1
        };

        // 1. The framing handshake: it must come before any content, and its reply must carry the
        //    tools the client wants to expose.
        self.server_message(&exec_server_field(1, "", 10, &[]))?;
        // 1b. The real service reads the replayed history back by id; ask for every blob.
        let blob_ids: Vec<Vec<u8>> = repeated(state, 1, 2).map(<[u8]>::to_vec).collect();
        for (index, blob_id) in blob_ids.iter().enumerate() {
            self.server_message(&kv_get_blob(100 + index as u64, blob_id))?;
        }
        // 2. A little text, the way the real service streams it.
        self.server_message(&text_delta(&format!("turn {turn_index}: ")))?;
        // 3. Then, first turn only, one MCP tool call and nothing else: the client is expected to
        //    end the turn with `toolUse`, run the tool itself, and come back with the result.
        if turn_index == 0 {
            if shape.tools.is_empty() {
                self.observed
                    .lock()
                    .unwrap()
                    .errors
                    .push("no MCP tools in the run request".into());
            }
            let args =
                json!({ "command": format!("echo hi > {}", self.script.lock().unwrap().file) });
            self.server_message(&exec_server_field(
                2,
                "exec-1",
                11,
                &mcp_args("zephyr_bash", "zephyr", "bash", "call-1", &args),
            ))?;
        } else {
            self.server_message(&turn_ended())?;
        }
        self.answered = true;
        Ok(())
    }

    fn server_message(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 5);
        frame.push(0);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        self.send.send_data(Bytes::from(frame), false)?;
        Ok(())
    }

    async fn finish(&mut self) -> anyhow::Result<()> {
        if !self.answered {
            self.observed
                .lock()
                .unwrap()
                .errors
                .push("a Run stream carried no run_request".into());
        }
        let _ = self.send.send_data(Bytes::new(), true);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AgentServerMessage builders
// ---------------------------------------------------------------------------

fn exec_server_field(id: u64, exec_id: &str, field_number: u32, payload: &[u8]) -> Vec<u8> {
    let mut message = varint(1, id);
    if !exec_id.is_empty() {
        message.extend(string(15, exec_id));
    }
    message.extend(len(field_number, payload));
    // AgentServerMessage.exec_server_message = 2
    len(2, &message)
}

fn kv_get_blob(id: u64, blob_id: &[u8]) -> Vec<u8> {
    // KvServerMessage{id = 1, get_blob_args = 2 {blob_id = 1}}
    let mut message = varint(1, id);
    message.extend(len(2, &len(1, blob_id)));
    // AgentServerMessage.kv_server_message = 4
    len(4, &message)
}

fn text_delta(text: &str) -> Vec<u8> {
    // InteractionUpdate.text_delta = 1 → TextDeltaUpdate.text = 1
    let update = len(1, &string(1, text));
    // AgentServerMessage.interaction_update = 1
    len(1, &update)
}

fn turn_ended() -> Vec<u8> {
    // InteractionUpdate.turn_ended = 14, with the token counters the real service fills in.
    let mut ended = varint(1, 11); // input_tokens
    ended.extend(varint(2, 7)); // output_tokens
    len(1, &len(14, &ended))
}

fn mcp_args(name: &str, provider: &str, tool: &str, call_id: &str, args: &Value) -> Vec<u8> {
    let mut message = string(1, name);
    // McpArgs.args is `map<string, google.protobuf.Value>`.
    let entry = {
        let mut entry = string(1, "command");
        entry.extend(len(2, &protobuf_value(args["command"].clone())));
        entry
    };
    message.extend(len(2, &entry));
    message.extend(string(3, call_id));
    message.extend(string(4, provider));
    message.extend(string(5, tool));
    message
}

/// `google.protobuf.Value` for the subset we use.
pub fn protobuf_value(value: Value) -> Vec<u8> {
    match value {
        Value::Null => varint(1, 0),
        Value::Number(number) => {
            let mut out = vec![0x11];
            out.extend_from_slice(&number.as_f64().unwrap_or_default().to_bits().to_le_bytes());
            out
        }
        Value::String(text) => string(3, &text),
        Value::Bool(flag) => varint(4, u64::from(flag)),
        Value::Object(map) => {
            let mut entries = Vec::new();
            for (key, value) in map {
                let mut entry = string(1, &key);
                entry.extend(len(2, &protobuf_value(value)));
                entries.extend(len(1, &entry));
            }
            len(5, &entries)
        }
        Value::Array(items) => {
            let mut values = Vec::new();
            for item in items {
                values.extend(len(1, &protobuf_value(item)));
            }
            len(6, &values)
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal protobuf wire helpers
// ---------------------------------------------------------------------------

fn varint(field: u32, value: u64) -> Vec<u8> {
    let mut out = encode_varint((field as u64) << 3);
    out.extend(encode_varint(value));
    out
}

fn string(field: u32, value: &str) -> Vec<u8> {
    len(field, value.as_bytes())
}

fn len(field: u32, body: &[u8]) -> Vec<u8> {
    let mut out = encode_varint((field as u64) << 3 | 2);
    out.extend(encode_varint(body.len() as u64));
    out.extend_from_slice(body);
    out
}

fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

struct Fields<'a> {
    bytes: &'a [u8],
    at: usize,
}

fn fields(bytes: &[u8]) -> Fields<'_> {
    Fields { bytes, at: 0 }
}

impl<'a> Iterator for Fields<'a> {
    /// `(field number, wire type, value bytes for LEN, little-endian value for varint)`.
    type Item = (u32, u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.bytes.len() {
            return None;
        }
        let (key, after_key) = read_varint(self.bytes, self.at)?;
        let wire = (key & 7) as u8;
        let field = (key >> 3) as u32;
        match wire {
            0 => {
                let (_, next) = read_varint(self.bytes, after_key)?;
                let value = &self.bytes[after_key..next];
                self.at = next;
                Some((field, wire, value))
            }
            2 => {
                let (size, start) = read_varint(self.bytes, after_key)?;
                let end = start.checked_add(size as usize)?;
                if end > self.bytes.len() {
                    return None;
                }
                let value = &self.bytes[start..end];
                self.at = end;
                Some((field, wire, value))
            }
            5 => {
                let end = after_key.checked_add(4)?;
                let value = &self.bytes[after_key..end];
                self.at = end;
                Some((field, wire, value))
            }
            1 => {
                let end = after_key.checked_add(8)?;
                let value = &self.bytes[after_key..end];
                self.at = end;
                Some((field, wire, value))
            }
            _ => None,
        }
    }
}

fn read_varint(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    let mut index = at;
    loop {
        let byte = *bytes.get(index)?;
        value |= ((byte & 0x7f) as u64) << shift;
        index += 1;
        if byte & 0x80 == 0 {
            return Some((value, index));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn first_field(bytes: &[u8]) -> Option<(u32, u8, &[u8])> {
    fields(bytes).next()
}

fn field(bytes: &[u8], number: u32, wire: u8) -> Option<&[u8]> {
    fields(bytes)
        .find(|(n, w, _)| *n == number && *w == wire)
        .map(|(_, _, v)| v)
}

fn has_field(bytes: &[u8], number: u32, wire: u8) -> bool {
    field(bytes, number, wire).is_some()
}

fn repeated(bytes: &[u8], number: u32, wire: u8) -> impl Iterator<Item = &[u8]> {
    fields(bytes)
        .filter(move |(n, w, _)| *n == number && *w == wire)
        .map(|(_, _, v)| v)
}

fn text(bytes: &[u8], number: u32) -> Option<String> {
    field(bytes, number, 2).map(|value| String::from_utf8_lossy(value).into_owned())
}

#[allow(dead_code)]
fn nested_text(bytes: &[u8], number: u32) -> Option<String> {
    field(bytes, number, 2).and_then(|inner| text(inner, 1))
}
