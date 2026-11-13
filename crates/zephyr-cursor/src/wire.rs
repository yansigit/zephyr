//! Wire format: the subset of `agent.v1` this provider needs, encoded by hand.
//!
//! The service speaks proto3 inside Connect frames over HTTP/2. Only ~15 messages matter, so the
//! encoders below write varints and length-delimited fields directly (`field_ld`, `field_str`,
//! `google_value`): no `prost`, no `build.rs`, no `protoc` in the image.
//!
//! Field numbers come from the published `agent.proto` (agent.v1); every constant below is a
//! citation of it. `tests/wire.rs` pins the bytes so a wrong field number fails loudly instead of
//! silently changing what the server sees.
use serde_json::Value;

/// Client version the service gates features on; a stale value gets `ERROR_UPDATE_REQUIRED`.
pub const CLIENT_VERSION: &str = "cli-2026.10.01-e373342";
/// Namespace for our tools' `provider_identifier`/name prefix. Cursor rejects MCP names that
/// collide with its own registry, so every advertised tool is `zephyr_<name>`.
pub const TOOL_PROVIDER: &str = "zephyr";
pub const TOOL_PREFIX: &str = "zephyr_";

/// One tool as Cursor sees it. `name` is the wire name (`zephyr_read`), `tool_name` the host's
/// (`read`), so the model's call can be mapped back without a table lookup at the call site.
#[derive(Clone, Debug, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub tool_name: String,
    pub provider_identifier: String,
    pub description: String,
    pub input_schema: Value,
}

/// What to do with the turn: send the new user text, or resume after a tool result.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    UserText { text: String, message_id: String },
    Resume,
}

/// One `AgentRunRequest`.
pub struct RunRequest<'a> {
    /// `ConversationStateStructure.root_prompt_messages_json`: blob ids of every history entry, in
    /// order. The server asks for each one with a KV `getBlob`.
    pub blobs: &'a [Vec<u8>],
    pub model: &'a str,
    pub conversation_id: &'a str,
    pub tools: &'a [McpTool],
    pub action: Action,
}

/// One decoded `AgentServerMessage`: the envelope's id/exec id plus the body we care about.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerMsg {
    /// `id` on `ExecServerMessage`/`KvServerMessage`; `0` for `InteractionUpdate`s (which carry
    /// none) — replies must echo it.
    pub id: u32,
    /// `ExecServerMessage.exec_id`, echoed in the reply.
    pub exec_id: String,
    pub body: ServerBody,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerBody {
    TextDelta(String),
    ThinkingDelta(String),
    /// Cumulative output tokens, as the engine reports them (`TokenDeltaUpdate.tokens`).
    Tokens(i64),
    /// `TurnEndedUpdate`: the authoritative totals for the turn (`input_tokens`, `output_tokens`,
    /// `cache_read_tokens`), reported once at the end. `TokenDeltaUpdate` only carries the running
    /// output count, so these win when present.
    TurnEnded {
        input: Option<i64>,
        output: Option<i64>,
        cache_read: Option<i64>,
    },
    Heartbeat,
    KvGetBlob {
        blob_id: Vec<u8>,
    },
    KvSetBlob {
        blob_id: Vec<u8>,
        data: Vec<u8>,
    },
    RequestContext,
    McpArgs {
        call_id: String,
        provider_identifier: String,
        tool_name: String,
        args: Value,
    },
    /// A native Cursor tool the model asked for. We advertise none, so this is answered with an
    /// error result (rather than left hanging) and the turn continues.
    RejectExec {
        kind: String,
    },
    /// `mcp_state_exec_args`: the service asks which MCP servers the client has; its
    /// `GetDynamicTools` meta-tool is answered from our reply, not from the request context.
    McpState,
    /// An exec member this crate has no shape for. Answered with an `ExecClientThrow`: a request
    /// left unanswered stalls the turn forever (the server keeps heartbeating).
    UnknownExec {
        member: u64,
    },
    Ignored,
}

// --- protobuf primitives --------------------------------------------------
// Adapted from 1jehuang/jcode (MIT), wire.rs

/// Depth cap for one `google.protobuf.Value` tree. Deeper input is refused instead of recursed, so
/// a hostile frame cannot run the decoder out of stack.
pub const MAX_VALUE_DEPTH: usize = 32;
/// Size cap for one `google.protobuf.Value`.
pub const MAX_VALUE_BYTES: usize = 10 * 1024 * 1024;

fn push_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn push_key(field: u64, wire: u8, out: &mut Vec<u8>) {
    push_varint((field << 3) | u64::from(wire), out);
}

/// One whole field as its own buffer, so nested messages are built bottom-up.
fn field_ld(field: u64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    push_key(field, 2, &mut out);
    push_varint(body.len() as u64, &mut out);
    out.extend_from_slice(body);
    out
}

fn field_str(field: u64, text: &str) -> Vec<u8> {
    field_ld(field, text.as_bytes())
}

fn field_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    push_key(field, 0, &mut out);
    push_varint(value, &mut out);
    out
}

fn field_fixed64(field: u64, value: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    push_key(field, 1, &mut out);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

/// One field as it sat on the wire: `data` for the length-delimited and fixed encodings, `varint`
/// for the varint one.
struct WireField<'a> {
    field: u64,
    wire: u8,
    varint: u64,
    data: &'a [u8],
}

fn read_varint(mut input: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for index in 0..10 {
        let (byte, rest) = input.split_first()?;
        value |= u64::from(byte & 0x7f) << (7 * index);
        input = rest;
        if byte & 0x80 == 0 {
            return Some((value, input));
        }
    }
    None
}

/// Splits a message body into its fields, stopping at the first byte that cannot start one. Every
/// decoder below is total: a truncated or hostile body yields the fields parsed so far.
fn wire_fields(input: &[u8]) -> Vec<WireField<'_>> {
    let mut fields = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let Some((key, after_key)) = read_varint(rest) else {
            break;
        };
        let field = key >> 3;
        let wire = (key & 0x7) as u8;
        // Field 0 does not exist; wire types 3/4 are the deprecated group encoding.
        if field == 0 || wire == 3 || wire == 4 || wire > 5 {
            break;
        }
        let parsed = match wire {
            0 => {
                let Some((varint, after)) = read_varint(after_key) else {
                    break;
                };
                rest = after;
                WireField {
                    field,
                    wire,
                    varint,
                    data: &[],
                }
            }
            1 | 5 => {
                let len = if wire == 1 { 8 } else { 4 };
                if after_key.len() < len {
                    break;
                }
                let (data, after) = after_key.split_at(len);
                rest = after;
                WireField {
                    field,
                    wire,
                    varint: 0,
                    data,
                }
            }
            _ => {
                let Some((len, after_len)) = read_varint(after_key) else {
                    break;
                };
                let Some(after_field) = after_len.get(usize::try_from(len).unwrap_or(usize::MAX)..)
                else {
                    break;
                };
                let data = &after_len[..after_len.len() - after_field.len()];
                rest = after_field;
                WireField {
                    field,
                    wire,
                    varint: 0,
                    data,
                }
            }
        };
        fields.push(parsed);
    }
    fields
}

/// Lossy UTF-8: a delta that is not valid UTF-8 still reaches the engine, mangled rather than lost.
fn text_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The `text = 1` of a `TextDeltaUpdate`/`ThinkingDeltaUpdate`; an update without one is empty
/// text rather than a dropped delta.
fn delta_text(bytes: &[u8]) -> String {
    wire_fields(bytes)
        .into_iter()
        .find(|field| field.field == 1 && field.wire == 2)
        .map_or_else(String::new, |field| text_of(field.data))
}

// --- `google.protobuf.Value` ----------------------------------------------
// Adapted from 1jehuang/jcode (MIT), wire.rs

/// `google.protobuf.Value` bytes for a JSON value (the `input_schema`/`args` carrier).
///
/// Past [`MAX_VALUE_DEPTH`] a nested value becomes `null`, and a result over [`MAX_VALUE_BYTES`]
/// is replaced by `null` wholesale, so the output is always a well-formed `Value` message.
pub fn encode_value(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_google_value(value, 0, &mut out);
    if out.len() > MAX_VALUE_BYTES {
        return field_varint(1, 0);
    }
    out
}

fn encode_google_value(value: &Value, depth: usize, out: &mut Vec<u8>) {
    if depth > MAX_VALUE_DEPTH {
        out.extend_from_slice(&field_varint(1, 0)); // Value.null_value = 1
        return;
    }
    match value {
        Value::Null => out.extend_from_slice(&field_varint(1, 0)),
        // Every JSON number travels as a double: `Value.number_value = 2`, wire type 1.
        Value::Number(number) => {
            out.extend_from_slice(&field_fixed64(2, number.as_f64().unwrap_or(0.0)));
        }
        Value::String(text) => out.extend_from_slice(&field_str(3, text)),
        Value::Bool(flag) => out.extend_from_slice(&field_varint(4, u64::from(*flag))),
        Value::Object(map) => {
            // `Value.struct_value = 5` -> Struct.fields = 1 map entry {key = 1, value = 2}.
            let mut structure = Vec::new();
            for (key, item) in map {
                let mut entry = field_str(1, key);
                let mut encoded = Vec::new();
                encode_google_value(item, depth + 1, &mut encoded);
                entry.extend_from_slice(&field_ld(2, &encoded));
                structure.extend_from_slice(&field_ld(1, &entry));
            }
            out.extend_from_slice(&field_ld(5, &structure));
        }
        Value::Array(items) => {
            // `Value.list_value = 6` -> ListValue.values = 1.
            let mut list = Vec::new();
            for item in items {
                let mut encoded = Vec::new();
                encode_google_value(item, depth + 1, &mut encoded);
                list.extend_from_slice(&field_ld(1, &encoded));
            }
            out.extend_from_slice(&field_ld(6, &list));
        }
    }
}

/// Inverse of [`encode_value`]. Malformed input decodes to `Value::Null` rather than failing the
/// stream: a tool call with unreadable args is still better surfaced than dropped.
pub fn decode_value(bytes: &[u8]) -> Value {
    decode_google_value(bytes, 0).unwrap_or(Value::Null)
}

fn decode_google_value(bytes: &[u8], depth: usize) -> Option<Value> {
    if depth > MAX_VALUE_DEPTH || bytes.len() > MAX_VALUE_BYTES {
        return None;
    }
    let mut value = None;
    for field in wire_fields(bytes) {
        match (field.field, field.wire) {
            (1, 0) => value = Some(Value::Null),
            (2, 1) => {
                let mut raw = [0u8; 8];
                raw.copy_from_slice(field.data);
                value = Some(json_number(f64::from_le_bytes(raw)));
            }
            (3, 2) => value = Some(Value::String(text_of(field.data))),
            (4, 0) => value = Some(Value::Bool(field.varint != 0)),
            (5, 2) => value = Some(Value::Object(decode_struct(field.data, depth)?)),
            (6, 2) => value = Some(Value::Array(decode_list(field.data, depth)?)),
            _ => {}
        }
    }
    value
}

/// A double that is exactly an `i64` comes back as a JSON integer, so tool args keep their type
/// through a round trip; anything else stays a float.
fn json_number(value: f64) -> Value {
    if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
    {
        return Value::from(value as i64);
    }
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn decode_struct(bytes: &[u8], depth: usize) -> Option<serde_json::Map<String, Value>> {
    let mut fields = serde_json::Map::new();
    for field in wire_fields(bytes) {
        if field.field != 1 || field.wire != 2 {
            continue;
        }
        let mut key = String::new();
        let mut value = Value::Null;
        for entry in wire_fields(field.data) {
            match (entry.field, entry.wire) {
                (1, 2) => key = text_of(entry.data),
                (2, 2) => value = decode_google_value(entry.data, depth + 1)?,
                _ => {}
            }
        }
        fields.insert(key, value);
    }
    Some(fields)
}

fn decode_list(bytes: &[u8], depth: usize) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    for field in wire_fields(bytes) {
        if field.field == 1 && field.wire == 2 {
            values.push(decode_google_value(field.data, depth + 1)?);
        }
    }
    Some(values)
}

// --- encoders -------------------------------------------------------------
// Each encoder returns one whole `AgentClientMessage` (or the reply inside it), ready for
// `wire::connect_frame`; the signatures are the contract `provider.rs` is written against.

/// One `AgentClientMessage{run_request}`.
pub fn encode_run_request(request: &RunRequest<'_>) -> Vec<u8> {
    // Adapted from 1jehuang/jcode (MIT), wire.rs
    let (thinking, fast) = crate::models::modes_of(request.model);

    // ConversationStateStructure.root_prompt_messages_json = 1. The other repeated fields (`turns`,
    // `todos`, `pending_tool_calls`, `previous_workspace_uris`, `summary_archives`,
    // `turn_timings`) are absent: proto3 reads a missing repeated field as empty, and this provider
    // replays the whole history as blobs, so there is nothing else to carry.
    let mut state = Vec::new();
    for blob_id in request.blobs {
        state.extend_from_slice(&field_ld(1, blob_id));
    }

    // ConversationAction: user_message_action = 1 {UserMessageAction.user_message = 1
    // {UserMessage.text = 1, .message_id = 2}}, or resume_action = 2 (an empty message).
    let action = match &request.action {
        Action::UserText { text, message_id } => {
            let mut user_message = field_str(1, text);
            if !message_id.is_empty() {
                user_message.extend_from_slice(&field_str(2, message_id));
            }
            field_ld(1, &field_ld(1, &user_message))
        }
        Action::Resume => field_ld(2, &[]),
    };

    // ModelDetails: model_id = 1, thinking_details = 2 (empty message: presence is the flag),
    // display_model_id = 3, display_name = 4. `max_mode` is deliberately absent: it is Cursor's
    // "Max Mode" plan switch, not a way to pick a faster model — the slug already does that.
    let display_name = crate::models::catalog()
        .iter()
        .find(|entry| entry.id == request.model)
        .map_or(request.model, |entry| entry.name.as_str());
    let mut model_details = field_str(1, request.model);
    if thinking {
        model_details.extend_from_slice(&field_ld(2, &[]));
    }
    model_details.extend_from_slice(&field_str(3, request.model));
    model_details.extend_from_slice(&field_str(4, display_name));

    // RequestedModel: model_id = 1, parameters = 3. A fast slug is the same base model with a
    // parameter, which is how the reference client sends it (`{id: "fast", value: "true"}`).
    let mut requested_model = field_str(1, request.model);
    if fast {
        let parameter = field_str(1, "fast");
        requested_model
            .extend_from_slice(&field_ld(3, &[parameter, field_str(2, "true")].concat()));
    }

    let mut run = field_ld(1, &state);
    run.extend_from_slice(&field_ld(2, &action));
    run.extend_from_slice(&field_ld(3, &model_details));
    if !request.tools.is_empty() {
        // McpTools.mcp_tools = 1, repeated McpToolDefinition.
        let mut tools = Vec::new();
        for tool in request.tools {
            tools.extend_from_slice(&field_ld(1, &encode_tool_definition(tool)));
        }
        run.extend_from_slice(&field_ld(4, &tools));
    }
    if !request.conversation_id.is_empty() {
        run.extend_from_slice(&field_str(5, request.conversation_id));
    }
    run.extend_from_slice(&field_ld(9, &requested_model));

    field_ld(1, &run) // AgentClientMessage.run_request = 1
}

/// `McpToolDefinition`: name = 1, description = 2, input_schema = 3, provider_identifier = 4,
/// tool_name = 5, input_schema_json = 6.
fn encode_tool_definition(tool: &McpTool) -> Vec<u8> {
    let mut definition = field_str(1, &tool.name);
    definition.extend_from_slice(&field_str(2, &tool.description));
    definition.extend_from_slice(&field_ld(3, &encode_value(&tool.input_schema)));
    definition.extend_from_slice(&field_str(4, &tool.provider_identifier));
    definition.extend_from_slice(&field_str(5, &tool.tool_name));
    // The same schema again as JSON text: the service reads this one for MCP tools.
    let schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
    definition.extend_from_slice(&field_str(6, &schema_json));
    definition
}

/// `AgentClientMessage{client_heartbeat}`. `ClientHeartbeat` is an empty message.
pub fn encode_client_heartbeat() -> Vec<u8> {
    field_ld(7, &[])
}

/// `KvClientMessage{id, get_blob_result{blob_data}}`.
pub fn encode_kv_get_blob_result(id: u32, data: &[u8]) -> Vec<u8> {
    // GetBlobResult.blob_data = 1 (optional: absent means "this blob does not exist").
    let result = if data.is_empty() {
        Vec::new()
    } else {
        field_ld(1, data)
    };
    let mut kv = Vec::new();
    if id != 0 {
        kv.extend_from_slice(&field_varint(1, u64::from(id)));
    }
    kv.extend_from_slice(&field_ld(2, &result));
    field_ld(3, &kv) // AgentClientMessage.kv_client_message = 3
}

/// `KvClientMessage{id, set_blob_result{}}`.
pub fn encode_kv_set_blob_ack(id: u32) -> Vec<u8> {
    let mut kv = Vec::new();
    if id != 0 {
        kv.extend_from_slice(&field_varint(1, u64::from(id)));
    }
    kv.extend_from_slice(&field_ld(3, &[])); // SetBlobResult: an empty message is a success
    field_ld(3, &kv)
}

/// `ExecClientMessage{id, exec_id, request_context_result{success{request_context{tools, env}}}}`.
pub fn encode_request_context_result(id: u32, exec_id: &str, tools: &[McpTool]) -> Vec<u8> {
    // RequestContextEnv: os_version = 1, shell = 3, time_zone = 10. `sandbox_enabled = 5` stays
    // off the wire (proto3 default false); workspace_paths = 2 is unknown here.
    let mut env = field_str(1, "linux");
    env.extend_from_slice(&field_str(3, "bash"));
    env.extend_from_slice(&field_str(10, "UTC"));

    // RequestContext: env = 4, tools = 7 (repeated McpToolDefinition).
    let mut context = Vec::new();
    if !tools.is_empty() {
        let mut definitions = Vec::new();
        for tool in tools {
            definitions.extend_from_slice(&field_ld(7, &encode_tool_definition(tool)));
        }
        context.extend_from_slice(&definitions);
    }
    context.extend_from_slice(&field_ld(4, &env));

    // RequestContextResult.success = 1 -> RequestContextSuccess.request_context = 1.
    let success = field_ld(1, &context);
    let result = field_ld(1, &success);

    let mut exec = Vec::new();
    if id != 0 {
        exec.extend_from_slice(&field_varint(1, u64::from(id)));
    }
    if !exec_id.is_empty() {
        exec.extend_from_slice(&field_str(15, exec_id));
    }
    exec.extend_from_slice(&field_ld(10, &result)); // ExecClientMessage.request_context_result = 10
    field_ld(2, &exec) // AgentClientMessage.exec_client_message = 2
}

/// `ExecClientMessage{id, exec_id, <kind>_result{error{...}}}`: refuse a native tool the model asked
/// for. `kind` is the proto oneof member we decoded (`shell`, `write`, `read`, ...).
pub fn encode_exec_rejection(id: u32, exec_id: &str, kind: &str) -> Vec<u8> {
    // Adapted from 1jehuang/jcode (MIT), wire.rs
    let reason = format!("not available: this session only wires up the {TOOL_PREFIX}* tools");
    let (result_field, payload) = rejection_payload(kind, &reason);

    let mut exec = Vec::new();
    if id != 0 {
        exec.extend_from_slice(&field_varint(1, u64::from(id)));
    }
    if !exec_id.is_empty() {
        exec.extend_from_slice(&field_str(15, exec_id));
    }
    exec.extend_from_slice(&field_ld(result_field, &payload));
    field_ld(2, &exec)
}

/// `ExecClientMessage{id, exec_id, mcp_state_exec_result{success{servers: [{zephyr, tools}]}}}`:
/// all our tools as one connected MCP server.
// Adapted from RoderAI/roder (MIT), proto_exec_mcp.rs
pub fn encode_mcp_state_result(id: u32, exec_id: &str, tools: &[McpTool]) -> Vec<u8> {
    // McpStateServer: server_name = 1, server_identifier = 2, tools = 5, status = 7.
    let mut server = field_str(1, TOOL_PROVIDER);
    server.extend_from_slice(&field_str(2, TOOL_PROVIDER));
    for tool in tools {
        server.extend_from_slice(&field_ld(5, &encode_tool_definition(tool)));
    }
    server.extend_from_slice(&field_str(7, "connected"));
    // McpStateExecResult.success = 1 -> McpStateSuccess.servers = 1.
    let result = field_ld(1, &field_ld(1, &server));

    let mut exec = Vec::new();
    if id != 0 {
        exec.extend_from_slice(&field_varint(1, u64::from(id)));
    }
    if !exec_id.is_empty() {
        exec.extend_from_slice(&field_str(15, exec_id));
    }
    exec.extend_from_slice(&field_ld(36, &result)); // ExecClientMessage.mcp_state_exec_result = 36
    field_ld(2, &exec)
}

/// `AgentClientMessage{exec_client_control_message{throw{id, error, error_code}}}`: the schema's
/// failure channel for an exec request this crate cannot answer on its own member.
// Adapted from RoderAI/roder (MIT), proto_exec_results.rs
pub fn encode_exec_throw(id: u32, member: u64) -> Vec<u8> {
    // ExecClientThrow: id = 1, error = 2, error_code = 4.
    let mut throw = field_varint(1, u64::from(id));
    throw.extend_from_slice(&field_str(
        2,
        &format!("unsupported exec request field {member}"),
    ));
    throw.extend_from_slice(&field_str(4, "UNSUPPORTED_EXEC_REQUEST"));
    // ExecClientControlMessage.throw = 2; AgentClientMessage.exec_client_control_message = 5.
    field_ld(5, &field_ld(2, &throw))
}

/// `(ExecClientMessage oneof member, <kind>_result body)` for one native tool. The member number is
/// the same on the request and the response; the inner case is whichever one carries an error.
fn rejection_payload(kind: &str, reason: &str) -> (u64, Vec<u8>) {
    match kind {
        // ShellResult.failure = 2, ShellFailure.stderr = 6. `rejected` would read as a user
        // refusal; a failure puts our text in front of the model so it can retry with our tools.
        "shell" => (2, field_ld(2, &field_str(6, reason))),
        // WriteResult.rejected = 6, WriteRejected.reason = 2.
        "write" => (3, field_ld(6, &field_str(2, reason))),
        // DeleteResult.rejected = 6, DeleteRejected.reason = 2.
        "delete" => (4, field_ld(6, &field_str(2, reason))),
        // GrepResult.error = 2, GrepError.error = 1.
        "grep" => (5, field_ld(2, &field_str(1, reason))),
        // ReadResult.error = 2, ReadError.error = 2.
        "read" => (7, field_ld(2, &field_str(2, reason))),
        // LsResult.error = 2, LsError.error = 2.
        "ls" => (8, field_ld(2, &field_str(2, reason))),
        // DiagnosticsResult.error = 2, DiagnosticsError.error = 2.
        "diagnostics" => (9, field_ld(2, &field_str(2, reason))),
        // ShellStream has no error case; `stderr` = 2 with ShellStreamStderr.data = 1.
        "shell_stream" => (14, field_ld(2, &field_str(1, reason))),
        // BackgroundShellSpawnResult.error = 2, BackgroundShellSpawnError.error = 3.
        "background_shell_spawn" => (16, field_ld(2, &field_str(3, reason))),
        // ListMcpResourcesExecResult.error = 2, ListMcpResourcesError.error = 1.
        "list_mcp_resources" => (17, field_ld(2, &field_str(1, reason))),
        // ReadMcpResourceExecResult.error = 2, ReadMcpResourceError.error = 2.
        "read_mcp_resource" => (18, field_ld(2, &field_str(2, reason))),
        // FetchResult.error = 2, FetchError.error = 2.
        "fetch" => (20, field_ld(2, &field_str(2, reason))),
        // RecordScreenResult.failure = 4, RecordScreenFailure.error = 1.
        "record_screen" => (21, field_ld(4, &field_str(1, reason))),
        // ComputerUseResult.error = 2, ComputerUseError.error = 1.
        "computer_use" => (22, field_ld(2, &field_str(1, reason))),
        // WriteShellStdinResult.error = 2, WriteShellStdinError.error = 1.
        "write_shell_stdin" => (23, field_ld(2, &field_str(1, reason))),
        // A member this crate has no result for still gets an answer: the shell failure is the one
        // shape every client can render, and the alternative is a call the server waits on.
        _ => (2, field_ld(2, &field_str(6, reason))),
    }
}

// --- decoders -------------------------------------------------------------
// Adapted from 1jehuang/jcode (MIT), wire.rs

/// One decoded `AgentServerMessage`.
pub fn decode_server_message(bytes: &[u8]) -> ServerMsg {
    let mut message = ServerMsg {
        id: 0,
        exec_id: String::new(),
        body: ServerBody::Ignored,
    };
    for field in wire_fields(bytes) {
        if field.wire != 2 {
            continue;
        }
        match field.field {
            1 => {
                if let Some(body) = decode_interaction_update(field.data) {
                    message.body = body;
                }
            }
            2 => decode_exec_server_message(field.data, &mut message), // exec_server_message
            4 => decode_kv_server_message(field.data, &mut message),   // kv_server_message
            _ => {} // checkpoints, control messages, queries: nothing this provider answers
        }
    }
    message
}

/// `InteractionUpdate`: text_delta = 1, thinking_delta = 4, token_delta = 8, heartbeat = 13,
/// turn_ended = 14. Everything else (tool-call bookkeeping, summaries) is engine-local noise.
fn decode_interaction_update(bytes: &[u8]) -> Option<ServerBody> {
    let mut body = None;
    for field in wire_fields(bytes) {
        match (field.field, field.wire) {
            // InteractionUpdate.text_delta = 1 / .thinking_delta = 4, each a delta update whose
            // own `text = 1` holds the characters.
            (1, 2) => body = Some(ServerBody::TextDelta(delta_text(field.data))),
            (4, 2) => body = Some(ServerBody::ThinkingDelta(delta_text(field.data))),
            // TokenDeltaUpdate.tokens = 1.
            (8, 2) => {
                let mut tokens = 0;
                for inner in wire_fields(field.data) {
                    if inner.field == 1 && inner.wire == 0 {
                        tokens = inner.varint as i64;
                    }
                }
                body = Some(ServerBody::Tokens(tokens));
            }
            (13, 2) => body = Some(ServerBody::Heartbeat),
            (14, 2) => {
                let (mut input, mut output, mut cache_read) = (None, None, None);
                for inner in wire_fields(field.data) {
                    if inner.wire != 0 {
                        continue;
                    }
                    match inner.field {
                        1 => input = Some(inner.varint as i64),
                        2 => output = Some(inner.varint as i64),
                        3 => cache_read = Some(inner.varint as i64),
                        _ => {}
                    }
                }
                body = Some(ServerBody::TurnEnded {
                    input,
                    output,
                    cache_read,
                });
            }
            _ => {}
        }
    }
    body
}

fn decode_exec_server_message(bytes: &[u8], message: &mut ServerMsg) {
    for field in wire_fields(bytes) {
        match (field.field, field.wire) {
            // ExecServerMessage.id = 1, .exec_id = 15.
            (1, 0) => message.id = field.varint as u32,
            (15, 2) => message.exec_id = text_of(field.data),
            // request_context_args = 10: the handshake that carries our tools.
            (10, 2) => message.body = ServerBody::RequestContext,
            (11, 2) => message.body = decode_mcp_args(field.data),
            (36, 2) => message.body = ServerBody::McpState,
            // span_context = 19 and machine_id = 57 sit beside the oneof, not in it.
            (19 | 57, _) => {}
            (member, 2) => {
                message.body = match native_exec_kind(member) {
                    Some(kind) => ServerBody::RejectExec {
                        kind: kind.to_string(),
                    },
                    None => ServerBody::UnknownExec { member },
                };
            }
            _ => {}
        }
    }
}

/// The native tools this provider answers (with a refusal): the `ExecServerMessage` oneof member
/// numbers that [`encode_exec_rejection`] has a matching `<kind>_result` for. Members without one
/// (hooks, subagents, allowlist prechecks) decode to [`ServerBody::UnknownExec`] and are answered
/// with a throw rather than a reply on the wrong oneof member.
fn native_exec_kind(member: u64) -> Option<&'static str> {
    Some(match member {
        2 => "shell",
        3 => "write",
        4 => "delete",
        5 => "grep",
        7 => "read",
        8 => "ls",
        9 => "diagnostics",
        14 => "shell_stream",
        16 => "background_shell_spawn",
        17 => "list_mcp_resources",
        18 => "read_mcp_resource",
        20 => "fetch",
        21 => "record_screen",
        22 => "computer_use",
        23 => "write_shell_stdin",
        _ => return None,
    })
}

/// `KvServerMessage`: id = 1, get_blob_args = 2 {blob_id = 1}, set_blob_args = 3 {blob_id = 1,
/// blob_data = 2}.
fn decode_kv_server_message(bytes: &[u8], message: &mut ServerMsg) {
    for field in wire_fields(bytes) {
        match (field.field, field.wire) {
            (1, 0) => message.id = field.varint as u32,
            (2, 2) => {
                let mut blob_id = Vec::new();
                for inner in wire_fields(field.data) {
                    if inner.field == 1 && inner.wire == 2 {
                        blob_id = inner.data.to_vec();
                    }
                }
                message.body = ServerBody::KvGetBlob { blob_id };
            }
            (3, 2) => {
                let mut blob_id = Vec::new();
                let mut data = Vec::new();
                for inner in wire_fields(field.data) {
                    match (inner.field, inner.wire) {
                        (1, 2) => blob_id = inner.data.to_vec(),
                        (2, 2) => data = inner.data.to_vec(),
                        _ => {}
                    }
                }
                message.body = ServerBody::KvSetBlob { blob_id, data };
            }
            _ => {}
        }
    }
}

/// `McpArgs`: name = 1, args = 2 (`map<string, bytes>`, each value a serialized
/// `google.protobuf.Value`), tool_call_id = 3, provider_identifier = 4, tool_name = 5.
fn decode_mcp_args(bytes: &[u8]) -> ServerBody {
    let mut name = String::new();
    let mut provider_identifier = String::new();
    let mut tool_name = String::new();
    let mut call_id = String::new();
    let mut entries = Vec::new();
    for field in wire_fields(bytes) {
        match (field.field, field.wire) {
            (1, 2) => name = text_of(field.data),
            (2, 2) => {
                if let Some(entry) = decode_arg_entry(field.data) {
                    entries.push(entry);
                }
            }
            (3, 2) => call_id = text_of(field.data),
            (4, 2) => provider_identifier = text_of(field.data),
            (5, 2) => tool_name = text_of(field.data),
            _ => {}
        }
    }
    if tool_name.is_empty() {
        // Some engines fill `name` and leave `tool_name` unset; the call still names our tool.
        tool_name = name;
    }
    ServerBody::McpArgs {
        call_id,
        provider_identifier,
        tool_name,
        args: args_object(entries),
    }
}

/// One `map<string, bytes>` entry: key = 1, value = 2.
fn decode_arg_entry(bytes: &[u8]) -> Option<(String, Value)> {
    let mut key = None;
    let mut value = None;
    for field in wire_fields(bytes) {
        match (field.field, field.wire) {
            (1, 2) => key = Some(text_of(field.data)),
            (2, 2) => value = Some(decode_arg_value(field.data)),
            _ => {}
        }
    }
    Some((key?, value.unwrap_or(Value::Null)))
}

/// A `McpArgs` value is a serialized `google.protobuf.Value`. A few engine builds send the raw JSON
/// instead, so bytes that decode to nothing readable are retried as JSON before giving up.
fn decode_arg_value(bytes: &[u8]) -> Value {
    match decode_google_value(bytes, 0) {
        None | Some(Value::Null) => serde_json::from_slice(bytes).unwrap_or(Value::Null),
        Some(value) => value,
    }
}

/// The tool arguments as one object. A single entry whose value is already an object *is* that
/// object (the shape the engines emit for MCP calls); anything else becomes the whole map.
fn args_object(entries: Vec<(String, Value)>) -> Value {
    if let [(_, Value::Object(object))] = entries.as_slice() {
        return Value::Object(object.clone());
    }
    let mut map = serde_json::Map::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Value::Object(map)
}

/// Endpoint for `Run`; `CURSOR_AGENT_URL` overrides it (tests point this at a local server).
pub fn endpoint() -> anyhow::Result<String> {
    agent_url(std::env::var("CURSOR_AGENT_URL").ok())
}

/// The agent URL, refusing one that would put the bearer token on the wire in clear: plain http
/// only to a loopback address (the test server), as `auth::base` does. Read with the transport's
/// own parser, so the host checked is the host connected to.
fn agent_url(over: Option<String>) -> anyhow::Result<String> {
    let Some(raw) = over else {
        return Ok("https://agentn.global.api5.cursor.sh/agent.v1.AgentService/Run".into());
    };
    let target = crate::transport::Target::parse(&raw)
        .map_err(|_| anyhow::anyhow!("CURSOR_AGENT_URL is not a valid URL"))?;
    if !target.tls && !crate::auth::loopback_host(&target.host) {
        anyhow::bail!("CURSOR_AGENT_URL must be https or a loopback address");
    }
    Ok(raw)
}

/// `[flag][len:u32 be][payload]`. The flag is `0` for the request (uncompressed) and `1` (gzip) or
/// `2` (JSON trailer) on the response.
pub fn connect_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Prefix of an id that [`safe_call_id`] had to encode.
const ENCODED_ID: &str = "zcx_";

/// Tool call ids must match `[A-Za-z0-9_-]`, but some engines return composites Cursor wants back
/// verbatim (OpenAI's `call_…\nfc_…`): an unsafe id becomes `zcx_<hex>`, which [`raw_call_id`]
/// reverses when the history is replayed.
pub fn safe_call_id(raw: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if raw.chars().all(safe) && !raw.starts_with(ENCODED_ID) {
        return raw.to_string();
    }
    let hex: String = raw.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{ENCODED_ID}{hex}")
}

/// The id Cursor issued for a call, from the one rpi carries (inverse of [`safe_call_id`]).
pub fn raw_call_id(id: &str) -> String {
    let decoded = id.strip_prefix(ENCODED_ID).and_then(|hex| {
        let bytes = hex.as_bytes();
        (bytes.len() % 2 == 0)
            .then(|| {
                bytes
                    .chunks(2)
                    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
                    .collect::<Option<Vec<u8>>>()
            })
            .flatten()
            .and_then(|raw| String::from_utf8(raw).ok())
    });
    decoded.unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_five_byte_headers_then_the_payload() {
        assert_eq!(connect_frame(b"hi"), vec![0, 0, 0, 0, 2, b'h', b'i']);
    }

    #[test]
    fn call_ids_are_restricted_to_the_schema_charset() {
        for raw in ["call-1\nfc_2", "tool_1-a", "zcx_41", ""] {
            let safe = safe_call_id(raw);
            assert!(safe
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
            assert_eq!(raw_call_id(&safe), raw);
        }
        assert_eq!(safe_call_id("tool_1-a"), "tool_1-a");
    }

    #[test]
    fn the_agent_url_is_https_or_loopback() {
        assert!(agent_url(None).unwrap().starts_with("https://"));
        for ok in [
            "https://agent.example/Run",
            "http://127.0.0.1:9/Run",
            "http://[::1]:9/Run",
            "http://localhost:9/Run",
        ] {
            assert_eq!(agent_url(Some(ok.into())).unwrap(), ok);
        }
        for bad in [
            "http://agent.example/Run",
            "http://10.0.0.1/Run",
            // The URL parser reads these hosts as 127.0.0.1; the transport would not.
            "http://127.0.0.1\\@evil.example/Run",
            "http://127.0.0.1#@evil.example/Run",
            "not a url",
        ] {
            assert!(agent_url(Some(bad.into())).is_err(), "{bad}");
        }
    }
}
