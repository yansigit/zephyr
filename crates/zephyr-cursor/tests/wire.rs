//! Golden bytes for `src/wire.rs`.
//!
//! Every expectation here is hand-computed from `agent.proto` field numbers and written out
//! literally (or walked with the independent parser below, never with the encoder's own helpers),
//! so a wrong constant fails here instead of quietly changing what the service sees.
//!
//! These are integration tests, so they reach the real encoders through the crate's public
//! modules (the cdylib also builds an rlib for exactly this).
use serde_json::{json, Value};
use zephyr_cursor::models;
use zephyr_cursor::wire::{
    connect_frame, decode_server_message, decode_value, encode_client_heartbeat,
    encode_exec_rejection, encode_kv_get_blob_result, encode_kv_set_blob_ack,
    encode_request_context_result, encode_run_request, encode_value, safe_call_id, Action, McpTool,
    RunRequest, ServerBody, MAX_VALUE_DEPTH,
};

// --- an independent protobuf reader ---------------------------------------
// Deliberately not the encoder's own helpers: a bug shared by both would otherwise pass.

#[derive(Debug, Clone, Copy)]
struct Field<'a> {
    number: u64,
    wire_type: u8,
    varint: u64,
    data: &'a [u8],
}

fn read_varint(input: &[u8]) -> (u64, &[u8]) {
    let mut value = 0u64;
    let mut shift = 0;
    for (index, byte) in input.iter().enumerate() {
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return (value, &input[index + 1..]);
        }
    }
    panic!("truncated varint in test input");
}

/// Splits a message into its fields, panicking on anything that is not valid proto3: the encoders
/// must never emit such a byte.
fn walk(mut input: &[u8]) -> Vec<Field<'_>> {
    let mut fields = Vec::new();
    while !input.is_empty() {
        let (key, rest) = read_varint(input);
        let number = key >> 3;
        let wire_type = (key & 0x7) as u8;
        assert!(number != 0, "field number 0 is not valid protobuf");
        let (field, rest) = match wire_type {
            0 => {
                let (value, rest) = read_varint(rest);
                (
                    Field {
                        number,
                        wire_type,
                        varint: value,
                        data: &[],
                    },
                    rest,
                )
            }
            1 => {
                let (data, rest) = rest.split_at(8);
                (
                    Field {
                        number,
                        wire_type,
                        varint: 0,
                        data,
                    },
                    rest,
                )
            }
            2 => {
                let (len, rest) = read_varint(rest);
                let (data, rest) = rest.split_at(usize::try_from(len).expect("length fits"));
                (
                    Field {
                        number,
                        wire_type,
                        varint: 0,
                        data,
                    },
                    rest,
                )
            }
            5 => {
                let (data, rest) = rest.split_at(4);
                (
                    Field {
                        number,
                        wire_type,
                        varint: 0,
                        data,
                    },
                    rest,
                )
            }
            other => panic!("unexpected wire type {other}"),
        };
        fields.push(field);
        input = rest;
    }
    fields
}

fn numbers(fields: &[Field<'_>]) -> Vec<u64> {
    fields.iter().map(|field| field.number).collect()
}

fn occurrences<'a>(fields: &[Field<'a>], number: u64) -> Vec<&'a [u8]> {
    fields
        .iter()
        .filter(|field| field.number == number && field.wire_type == 2)
        .map(|field| field.data)
        .collect()
}

/// The one length-delimited occurrence of `number`, asserted to be unique.
fn ld_at<'a>(fields: &[Field<'a>], number: u64) -> &'a [u8] {
    let found = occurrences(fields, number);
    assert_eq!(
        found.len(),
        1,
        "field {number} should appear exactly once in {:?}",
        numbers(fields)
    );
    found[0]
}

fn varint_at(fields: &[Field<'_>], number: u64) -> u64 {
    let found: Vec<&Field<'_>> = fields
        .iter()
        .filter(|field| field.number == number)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "field {number} should appear exactly once in {:?}",
        numbers(fields)
    );
    assert_eq!(found[0].wire_type, 0, "field {number} should be a varint");
    found[0].varint
}

fn text(data: &[u8]) -> &str {
    std::str::from_utf8(data).expect("valid utf-8")
}

// --- hand-built protobuf, for the frames the server would send -------------

fn push_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn ld(number: u64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_varint((number << 3) | 2, &mut out);
    push_varint(body.len() as u64, &mut out);
    out.extend_from_slice(body);
    out
}

fn varint(number: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    push_varint(number << 3, &mut out);
    push_varint(value, &mut out);
    out
}

fn text_field(number: u64, value: &str) -> Vec<u8> {
    ld(number, value.as_bytes())
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

fn tool(name: &str, tool_name: &str, schema: Value) -> McpTool {
    McpTool {
        name: name.to_string(),
        tool_name: tool_name.to_string(),
        provider_identifier: "zephyr".to_string(),
        description: format!("{tool_name} the {name}"),
        input_schema: schema,
    }
}

fn request<'a>(blobs: &'a [Vec<u8>], tools: &'a [McpTool], action: Action) -> RunRequest<'a> {
    RunRequest {
        blobs,
        model: "composer-2-fast",
        conversation_id: "conv-1",
        tools,
        action,
    }
}

// --- frames ---------------------------------------------------------------

#[test]
fn frames_are_a_flag_then_a_big_endian_length() {
    assert_eq!(connect_frame(b"hi"), vec![0, 0, 0, 0, 2, b'h', b'i']);
    assert_eq!(connect_frame(&[]), vec![0, 0, 0, 0, 0]);
}

#[test]
fn call_ids_keep_only_the_schema_charset() {
    assert_eq!(safe_call_id("call-1\nfc_2"), "call-1_fc_2");
}

// --- google.protobuf.Value ------------------------------------------------

#[test]
fn value_bytes_for_each_json_kind_are_hand_computed() {
    // Value.null_value = 1 (varint).
    assert_eq!(encode_value(&Value::Null), vec![0x08, 0x00]);
    // Value.number_value = 2 (fixed64); 1.5 == 0x3ff8_0000_0000_0000 big endian.
    assert_eq!(
        encode_value(&json!(1.5)),
        vec![0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf8, 0x3f]
    );
    // Value.string_value = 3.
    assert_eq!(encode_value(&json!("hi")), vec![0x1a, 0x02, b'h', b'i']);
    // Value.bool_value = 4.
    assert_eq!(encode_value(&json!(true)), vec![0x20, 0x01]);
    // Value.list_value = 6 -> ListValue.values = 1 -> number 1.0.
    assert_eq!(
        encode_value(&json!([1])),
        vec![0x32, 0x0b, 0x0a, 0x09, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f]
    );
    // Value.struct_value = 5 -> Struct.fields = 1 -> {key = 1, value = 2}: {"a": true}.
    assert_eq!(
        encode_value(&json!({"a": true})),
        vec![0x2a, 0x09, 0x0a, 0x07, 0x0a, 0x01, b'a', 0x12, 0x02, 0x20, 0x01]
    );
}

#[test]
fn values_round_trip_including_nested_objects_and_arrays() {
    let value = json!({
        "path": "/tmp/curwire",
        "lines": [1, 2.5, -3, "four", null, true],
        "options": {"recursive": false, "depth": 3, "nested": {"list": [{"a": 1}]}},
    });
    assert_eq!(decode_value(&encode_value(&value)), value);
    for scalar in [
        Value::Null,
        json!(""),
        json!(0),
        json!(-1.25),
        json!(false),
        json!([]),
        json!({}),
    ] {
        assert_eq!(
            decode_value(&encode_value(&scalar)),
            scalar,
            "round trip of {scalar}"
        );
    }
}

#[test]
fn malformed_value_bytes_decode_to_null_instead_of_panicking() {
    assert_eq!(decode_value(&[]), Value::Null);
    assert_eq!(decode_value(&[0x1a]), Value::Null);
    assert_eq!(decode_value(&[0x1a, 0x40, b'h']), Value::Null);
    assert_eq!(decode_value(&[0xff, 0xff, 0xff]), Value::Null);
}

#[test]
fn deeply_nested_values_are_capped_at_the_depth_limit() {
    // `MAX_VALUE_DEPTH` nested lists decode; one more does not reach the innermost value and the
    // whole tree collapses to null rather than recursing into a stack overflow.
    let mut within = encode_value(&Value::Null);
    let mut over = within.clone();
    for _ in 0..MAX_VALUE_DEPTH {
        within = ld(6, &ld(1, &within));
    }
    for _ in 0..=MAX_VALUE_DEPTH {
        over = ld(6, &ld(1, &over));
    }
    assert!(
        decode_value(&within).is_array(),
        "depth {MAX_VALUE_DEPTH} still decodes"
    );
    assert_eq!(
        decode_value(&over),
        Value::Null,
        "depth {} is refused",
        MAX_VALUE_DEPTH + 1
    );

    // Encoding a tree that deep terminates as well, and nothing panics.
    let mut deep = Value::Null;
    for _ in 0..(MAX_VALUE_DEPTH * 4) {
        deep = Value::Array(vec![deep]);
    }
    let _ = encode_value(&deep);
}

// --- AgentRunRequest ------------------------------------------------------

#[test]
fn a_turn_is_one_run_request_whose_top_level_field_is_one() {
    let blobs = vec![b"blob-a".to_vec(), b"blob-b".to_vec()];
    let tools = vec![tool("zephyr_read", "read", json!({"type": "object"}))];
    let bytes = encode_run_request(&request(
        &blobs,
        &tools,
        Action::UserText {
            text: "hello".to_string(),
            message_id: "msg-1".to_string(),
        },
    ));

    // AgentClientMessage.run_request = 1.
    let top = walk(&bytes);
    assert_eq!(numbers(&top), vec![1]);
    let run = walk(ld_at(&top, 1));

    // AgentRunRequest: conversation_state = 1, action = 2, model_details = 3, mcp_tools = 4,
    // conversation_id = 5, requested_model = 9.
    assert_eq!(numbers(&run), vec![1, 2, 3, 4, 5, 9]);

    // ConversationStateStructure.root_prompt_messages_json = 1, once per history entry.
    let state = walk(ld_at(&run, 1));
    assert_eq!(numbers(&state), vec![1, 1]);
    assert_eq!(
        occurrences(&state, 1),
        vec![b"blob-a".as_slice(), b"blob-b".as_slice()]
    );

    // ConversationAction.user_message_action = 1 -> UserMessageAction.user_message = 1 ->
    // UserMessage.text = 1, .message_id = 2.
    let action = walk(ld_at(&run, 2));
    assert_eq!(numbers(&action), vec![1]);
    let user_message = walk(ld_at(&walk(ld_at(&action, 1)), 1));
    assert_eq!(numbers(&user_message), vec![1, 2]);
    assert_eq!(text(ld_at(&user_message, 1)), "hello");
    assert_eq!(text(ld_at(&user_message, 2)), "msg-1");

    // ModelDetails.model_id = 1, .display_model_id = 3, .display_name = 4 (the catalog's name for
    // the id, so the request never depends on the caller spelling a model).
    let details = walk(ld_at(&run, 3));
    assert_eq!(text(ld_at(&details, 1)), "composer-2-fast");
    assert_eq!(text(ld_at(&details, 3)), "composer-2-fast");
    let display = models::catalog()
        .iter()
        .find(|model| model.id == "composer-2-fast")
        .expect("catalogued")
        .name
        .clone();
    assert_eq!(text(ld_at(&details, 4)), display);

    // McpTools.mcp_tools = 1, repeated McpToolDefinition.
    let tools_field = walk(ld_at(&run, 4));
    assert_eq!(numbers(&tools_field), vec![1]);
    let definition = walk(ld_at(&tools_field, 1));
    assert_eq!(text(ld_at(&definition, 1)), "zephyr_read"); // .name
    assert_eq!(text(ld_at(&definition, 2)), "read the zephyr_read"); // .description
    assert_eq!(
        decode_value(ld_at(&definition, 3)),
        json!({"type": "object"})
    ); // .input_schema
    assert_eq!(text(ld_at(&definition, 4)), "zephyr"); // .provider_identifier
    assert_eq!(text(ld_at(&definition, 5)), "read"); // .tool_name
    assert_eq!(text(ld_at(&definition, 6)), r#"{"type":"object"}"#); // .input_schema_json

    // AgentRunRequest.conversation_id = 5.
    assert_eq!(text(ld_at(&run, 5)), "conv-1");

    // RequestedModel.model_id = 1.
    let requested = walk(ld_at(&run, 9));
    assert_eq!(text(ld_at(&requested, 1)), "composer-2-fast");
}

#[test]
fn every_tool_gets_its_own_definition_entry() {
    let blobs: Vec<Vec<u8>> = Vec::new();
    let tools = vec![
        tool(
            "zephyr_read",
            "read",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        ),
        tool("zephyr_bash", "bash", json!({"type": "object"})),
    ];
    let bytes = encode_run_request(&request(&blobs, &tools, Action::Resume));
    let run = walk(ld_at(&walk(&bytes), 1));

    let definitions = occurrences(&walk(ld_at(&run, 4)), 1);
    let names: Vec<String> = definitions
        .iter()
        .map(|entry| text(ld_at(&walk(entry), 1)).to_string())
        .collect();
    assert_eq!(
        names,
        vec!["zephyr_read".to_string(), "zephyr_bash".to_string()]
    );
    let tool_names: Vec<String> = definitions
        .iter()
        .map(|entry| text(ld_at(&walk(entry), 5)).to_string())
        .collect();
    assert_eq!(tool_names, vec!["read".to_string(), "bash".to_string()]);

    // ResumeAction: ConversationAction.resume_action = 2, an empty message.
    let action = walk(ld_at(&run, 2));
    assert_eq!(numbers(&action), vec![2]);
    assert!(ld_at(&action, 2).is_empty());
    // No blobs, but a conversation id and two tools: only the state field is left empty.
    assert_eq!(numbers(&run), vec![1, 2, 3, 4, 5, 9]);
    assert!(ld_at(&run, 1).is_empty());
}

#[test]
fn an_empty_conversation_id_and_tool_list_stay_off_the_wire() {
    let blobs = vec![b"only".to_vec()];
    let bytes = encode_run_request(&RunRequest {
        blobs: &blobs,
        model: "gpt-5",
        conversation_id: "",
        tools: &[],
        action: Action::UserText {
            text: "hi".to_string(),
            message_id: String::new(),
        },
    });
    let run = walk(ld_at(&walk(&bytes), 1));
    assert_eq!(numbers(&run), vec![1, 2, 3, 9]);
    // A missing message id is absent, not empty.
    let user_message = walk(ld_at(&walk(ld_at(&walk(ld_at(&run, 2)), 1)), 1));
    assert_eq!(numbers(&user_message), vec![1]);
}

#[test]
fn thinking_and_fast_flags_come_from_the_model_id() {
    // ModelDetails.thinking_details = 2 is an empty message: presence is the flag.
    let blobs: Vec<Vec<u8>> = Vec::new();
    let thinking = encode_run_request(&RunRequest {
        blobs: &blobs,
        model: "sonnet-4.6-thinking",
        conversation_id: "conv-2",
        tools: &[],
        action: Action::Resume,
    });
    let run = walk(ld_at(&walk(&thinking), 1));
    let details = walk(ld_at(&run, 3));
    assert!(
        ld_at(&details, 2).is_empty(),
        "thinking_details should be present but empty"
    );
    assert!(
        !numbers(&details).contains(&7),
        "max_mode is Cursor's plan switch, never set here"
    );
    assert!(
        !numbers(&walk(ld_at(&run, 9))).contains(&2),
        "requested max_mode stays unset"
    );
    assert!(
        !numbers(&walk(ld_at(&run, 9))).contains(&3),
        "a thinking id asks for no model parameter"
    );

    // A fast id carries RequestedModel.parameters = [{id: "fast", value: "true"}], and neither
    // details nor the requested model is in max mode.
    let bytes = encode_run_request(&request(&blobs, &[], Action::Resume));
    let run = walk(ld_at(&walk(&bytes), 1));
    let details = walk(ld_at(&run, 3));
    assert!(
        !numbers(&details).contains(&2),
        "composer-2-fast is not a thinking id"
    );
    assert!(!numbers(&details).contains(&7));
    let requested = walk(ld_at(&run, 9));
    assert!(!numbers(&requested).contains(&2));
    let parameter = walk(ld_at(&requested, 3));
    assert_eq!(text(ld_at(&parameter, 1)), "fast");
    assert_eq!(text(ld_at(&parameter, 2)), "true");
}

// --- the mid-stream replies ----------------------------------------------

#[test]
fn the_heartbeat_is_a_bare_seven_field_message() {
    // AgentClientMessage.client_heartbeat = 7, empty ClientHeartbeat.
    assert_eq!(encode_client_heartbeat(), vec![0x3a, 0x00]);
}

#[test]
fn kv_replies_echo_the_id_and_the_blob() {
    // AgentClientMessage.kv_client_message = 3.
    let reply = encode_kv_get_blob_result(7, b"blob-7");
    let top = walk(&reply);
    assert_eq!(numbers(&top), vec![3]);
    let kv = walk(ld_at(&top, 3));
    assert_eq!(varint_at(&kv, 1), 7); // KvClientMessage.id = 1
                                      // KvClientMessage.get_blob_result = 2 -> GetBlobResult.blob_data = 1.
    assert_eq!(ld_at(&walk(ld_at(&kv, 2)), 1), b"blob-7");

    // A missing blob is an empty result, which the server reads as "not found".
    let missing = encode_kv_get_blob_result(8, b"");
    let missing = walk(&missing);
    assert_eq!(varint_at(&walk(ld_at(&missing, 3)), 1), 8);
    assert!(ld_at(&walk(ld_at(&missing, 3)), 2).is_empty());

    // KvClientMessage.set_blob_result = 3, an empty message is the ack.
    let ack = encode_kv_set_blob_ack(9);
    let ack = walk(&ack);
    assert_eq!(numbers(&ack), vec![3]);
    let kv = walk(ld_at(&ack, 3));
    assert_eq!(varint_at(&kv, 1), 9);
    assert!(ld_at(&kv, 3).is_empty());
}

#[test]
fn the_request_context_reply_carries_our_tools_and_env() {
    let tools = vec![
        tool("zephyr_read", "read", json!({"type": "object"})),
        tool(
            "zephyr_edit",
            "edit",
            json!({"type": "object", "properties": {}}),
        ),
    ];
    let reply = encode_request_context_result(3, "exec-3", &tools);
    let top = walk(&reply);
    assert_eq!(numbers(&top), vec![2]); // AgentClientMessage.exec_client_message = 2
    let exec = walk(ld_at(&top, 2));

    assert_eq!(numbers(&exec), vec![1, 15, 10]);
    assert_eq!(varint_at(&exec, 1), 3); // ExecClientMessage.id = 1
    assert_eq!(text(ld_at(&exec, 15)), "exec-3"); // .exec_id = 15
                                                  // RequestContextResult.success = 1 -> RequestContextSuccess.request_context = 1.
    let success = walk(ld_at(&exec, 10));
    let context = walk(ld_at(&walk(ld_at(&success, 1)), 1));

    // RequestContext.tools = 7, repeated McpToolDefinition.
    let definitions = occurrences(&context, 7);
    assert_eq!(definitions.len(), 2);
    let first = walk(definitions[0]);
    assert_eq!(text(ld_at(&first, 1)), "zephyr_read");
    assert_eq!(text(ld_at(&first, 5)), "read");
    assert_eq!(text(ld_at(&first, 4)), "zephyr");
    assert_eq!(decode_value(ld_at(&first, 3)), json!({"type": "object"}));
    assert_eq!(text(ld_at(&first, 6)), r#"{"type":"object"}"#);

    // RequestContext.env = 4 -> RequestContextEnv.os_version = 1, .shell = 3, .time_zone = 10.
    let env = walk(ld_at(&context, 4));
    assert_eq!(text(ld_at(&env, 1)), "linux");
    assert_eq!(text(ld_at(&env, 3)), "bash");
    assert_eq!(text(ld_at(&env, 10)), "UTC");
    assert!(
        !numbers(&env).contains(&5),
        "sandbox_enabled is the proto3 default and stays off"
    );
}

#[test]
fn a_rejection_answers_the_same_oneof_member_it_was_asked_on() {
    // (ExecServerMessage request member, the kind name we decode it to).
    let members = [
        (2, "shell"),
        (3, "write"),
        (4, "delete"),
        (5, "grep"),
        (7, "read"),
        (8, "ls"),
        (9, "diagnostics"),
        (14, "shell_stream"),
        (16, "background_shell_spawn"),
        (17, "list_mcp_resources"),
        (18, "read_mcp_resource"),
        (20, "fetch"),
        (21, "record_screen"),
        (22, "computer_use"),
        (23, "write_shell_stdin"),
    ];
    for (member, kind) in members {
        let frame = ld(
            2, // AgentServerMessage.exec_server_message = 2
            &concat(&[
                varint(1, 5),             // ExecServerMessage.id = 1
                text_field(15, "exec-5"), // .exec_id = 15
                ld(member, &[]),          // the native tool's args, which we do not need
            ]),
        );
        let message = decode_server_message(&frame);
        assert_eq!(message.id, 5);
        assert_eq!(message.exec_id, "exec-5");
        assert_eq!(
            message.body,
            ServerBody::RejectExec {
                kind: kind.to_string()
            },
            "member {member}"
        );

        let reply = encode_exec_rejection(message.id, &message.exec_id, kind);
        let top = walk(&reply);
        assert_eq!(numbers(&top), vec![2]);
        let exec = walk(ld_at(&top, 2));
        assert_eq!(varint_at(&exec, 1), 5);
        assert_eq!(text(ld_at(&exec, 15)), "exec-5");
        assert_eq!(
            numbers(&exec)
                .iter()
                .copied()
                .filter(|n| *n != 1 && *n != 15)
                .collect::<Vec<u64>>(),
            vec![member],
            "the {kind} reply must answer on the member it was asked on"
        );
    }
}

#[test]
fn an_unknown_native_kind_still_produces_a_valid_shell_failure() {
    let reply = encode_exec_rejection(6, "exec-6", "subagent");
    let exec = walk(ld_at(&walk(&reply), 2));
    assert_eq!(numbers(&exec), vec![1, 15, 2]);
    // ShellResult.failure = 2 -> ShellFailure.stderr = 6.
    let failure = walk(ld_at(&walk(ld_at(&exec, 2)), 2));
    assert!(text(ld_at(&failure, 6)).contains("zephyr_"));
}

// --- AgentServerMessage ---------------------------------------------------

fn interaction(member: u64, body: &[u8]) -> Vec<u8> {
    // AgentServerMessage.interaction_update = 1.
    ld(1, &ld(member, body))
}

fn exec_message(member: u64, body: &[u8]) -> Vec<u8> {
    concat(&[ld(
        2,
        &concat(&[varint(1, 3), text_field(15, "exec-3"), ld(member, body)]),
    )])
}

#[test]
fn interaction_updates_decode_to_their_bodies() {
    // TextDeltaUpdate.text = 1.
    assert_eq!(
        decode_server_message(&interaction(1, &text_field(1, "hey"))).body,
        ServerBody::TextDelta("hey".to_string())
    );
    // ThinkingDeltaUpdate.text = 1.
    assert_eq!(
        decode_server_message(&interaction(4, &text_field(1, "hmm"))).body,
        ServerBody::ThinkingDelta("hmm".to_string())
    );
    // TokenDeltaUpdate.tokens = 1.
    assert_eq!(
        decode_server_message(&interaction(8, &varint(1, 42))).body,
        ServerBody::Tokens(42)
    );
    // HeartbeatUpdate / TurnEndedUpdate are empty.
    assert_eq!(
        decode_server_message(&interaction(13, &[])).body,
        ServerBody::Heartbeat
    );
    // TurnEndedUpdate carries the turn's totals (input = 1, output = 2, cache_read = 3); an empty
    // one leaves them unset rather than reporting zero.
    assert_eq!(
        decode_server_message(&interaction(14, &[])).body,
        ServerBody::TurnEnded {
            input: None,
            output: None,
            cache_read: None
        }
    );
    assert_eq!(
        decode_server_message(&interaction(
            14,
            &concat(&[varint(1, 11), varint(2, 7), varint(3, 5)])
        ))
        .body,
        ServerBody::TurnEnded {
            input: Some(11),
            output: Some(7),
            cache_read: Some(5)
        }
    );
    // An interaction update carries no id.
    assert_eq!(decode_server_message(&interaction(14, &[])).id, 0);
}

#[test]
fn a_gzipped_frame_decodes_once_the_transport_has_inflated_it() {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let frame = interaction(1, &text_field(1, "compressed"));
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &frame).expect("gzip");
    let gzipped = gz.finish().expect("gzip");

    // Decompression belongs to the transport; this pins that decode only ever sees the inflated
    // bytes and that the result is unchanged.
    let mut inflated = Vec::new();
    GzDecoder::new(gzipped.as_slice())
        .read_to_end(&mut inflated)
        .expect("gunzip");
    assert_eq!(&inflated, &frame);
    assert_eq!(
        decode_server_message(&inflated).body,
        ServerBody::TextDelta("compressed".to_string())
    );
    // Bytes that are still compressed are not a proto message: they decode to something harmless.
    let _ = decode_server_message(&gzipped);
}

#[test]
fn kv_messages_decode_with_their_ids_and_blobs() {
    // KvServerMessage.id = 1; get_blob_args = 2 {blob_id = 1}.
    let get = concat(&[varint(1, 7), ld(2, &ld(1, b"blob-7"))]);
    let message = decode_server_message(&ld(4, &get));
    assert_eq!(message.id, 7);
    assert_eq!(
        message.body,
        ServerBody::KvGetBlob {
            blob_id: b"blob-7".to_vec()
        }
    );

    // set_blob_args = 3 {blob_id = 1, blob_data = 2}.
    let set = concat(&[
        varint(1, 8),
        ld(3, &concat(&[ld(1, b"blob-8"), ld(2, b"payload")])),
    ]);
    let message = decode_server_message(&ld(4, &set));
    assert_eq!(message.id, 8);
    assert_eq!(
        message.body,
        ServerBody::KvSetBlob {
            blob_id: b"blob-8".to_vec(),
            data: b"payload".to_vec()
        }
    );
}

#[test]
fn the_request_context_handshake_decodes() {
    let message = decode_server_message(&exec_message(10, &[]));
    assert_eq!(message.body, ServerBody::RequestContext);
    assert_eq!(message.id, 3);
    assert_eq!(message.exec_id, "exec-3");
}

#[test]
fn mcp_args_decode_into_one_object_of_tool_arguments() {
    // McpArgs: name = 1, args = 2 (map entry {key = 1, value = 2 (a serialized Value)}),
    // tool_call_id = 3, provider_identifier = 4, tool_name = 5.
    let arguments = encode_value(&json!({"path": "/tmp/curwire", "limit": 20}));
    let entry = concat(&[text_field(1, "arguments"), ld(2, &arguments)]);
    let body = concat(&[
        text_field(1, "mcp_zephyr_read"),
        ld(2, &entry),
        text_field(3, "call-1"),
        text_field(4, "zephyr"),
        text_field(5, "read"),
    ]);
    let message = decode_server_message(&exec_message(11, &body));
    assert_eq!(
        message.body,
        ServerBody::McpArgs {
            call_id: "call-1".to_string(),
            provider_identifier: "zephyr".to_string(),
            tool_name: "read".to_string(),
            // A single entry whose value is an object *is* the arguments.
            args: json!({"path": "/tmp/curwire", "limit": 20}),
        }
    );

    // Several entries have no single object to unwrap: the whole map is the argument object.
    let two = concat(&[
        ld(
            2,
            &concat(&[text_field(1, "a"), ld(2, &encode_value(&json!(1)))]),
        ),
        ld(
            2,
            &concat(&[text_field(1, "b"), ld(2, &encode_value(&json!("two")))]),
        ),
    ]);
    let message = decode_server_message(&exec_message(11, &two));
    match message.body {
        ServerBody::McpArgs {
            args, tool_name, ..
        } => {
            assert_eq!(args, json!({"a": 1, "b": "two"}));
            assert_eq!(
                tool_name, "",
                "no tool_name and no name is an empty name, not a panic"
            );
        }
        other => panic!("expected mcp args, got {other:?}"),
    }

    // `name` fills in when the engine leaves `tool_name` unset.
    let named = concat(&[text_field(1, "zephyr_read"), text_field(3, "call-2")]);
    match decode_server_message(&exec_message(11, &named)).body {
        ServerBody::McpArgs {
            tool_name,
            call_id,
            args,
            ..
        } => {
            assert_eq!(tool_name, "zephyr_read");
            assert_eq!(call_id, "call-2");
            assert_eq!(args, json!({}));
        }
        other => panic!("expected mcp args, got {other:?}"),
    }
}

#[test]
fn unknown_envelopes_and_members_are_ignored() {
    // AgentServerMessage has no field 9: an unknown envelope must not be guessed at.
    assert_eq!(
        decode_server_message(&ld(9, b"whatever")).body,
        ServerBody::Ignored
    );
    // ExecServerMessage.subagent_args = 28 is a native call this crate has no result shape for.
    assert_eq!(
        decode_server_message(&exec_message(28, b"")).body,
        ServerBody::Ignored
    );
    // InteractionUpdate.tool_call_started = 2 is engine bookkeeping, not a body we surface.
    assert_eq!(
        decode_server_message(&interaction(2, b"")).body,
        ServerBody::Ignored
    );
    assert_eq!(decode_server_message(&[]).body, ServerBody::Ignored);
}

#[test]
fn truncated_input_never_panics() {
    let frame = concat(&[
        ld(1, &ld(1, &text_field(1, "hello"))),
        ld(4, &concat(&[varint(1, 7), ld(2, &ld(1, b"blob-7"))])),
    ]);
    for end in 0..frame.len() {
        // Any prefix is a valid input: the decoders are total, so this only has to not panic.
        let _ = decode_server_message(&frame[..end]);
        let _ = decode_value(&frame[..end]);
    }
    let message = decode_server_message(&frame[..frame.len() - 1]);
    assert!(matches!(
        message.body,
        ServerBody::TextDelta(_) | ServerBody::KvGetBlob { .. } | ServerBody::Ignored
    ));
}
