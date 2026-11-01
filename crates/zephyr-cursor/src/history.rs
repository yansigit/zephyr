//! Turning rpi's `Context` into what the agent service replays: one JSON entry per message, plus
//! this turn's action.
//!
//! The service has no conversation of its own that survives a turn (each `Run` call carries the
//! whole history as `ConversationStateStructure.root_prompt_messages_json`), so the provider rebuilds
//! it every time from `ctx.messages`. Two shapes matter:
//!
//! * its role vocabulary is `user` / `assistant` / `tool` — a `system` role is dropped, so the
//!   system prompt is sent as the operator's first exchange instead, and
//! * tool results are their own entries addressed by `toolCallId`, which is why the ids emitted in
//!   `provider.rs` must be the ones replayed here verbatim.
use crate::wire;
use rpi_ai::{Content, Context, Message, UserContent};
use serde_json::{json, Value};

/// A stateless replay has no server-side turn to resume from when the history ends on tool results
/// (or on an assistant message), so the caller sends this instead of an empty user message.
pub const CONTINUATION_PROMPT: &str = "Continue, using the tool results above.";

/// The system prompt has nowhere to go — the service drops a `system` role — so it opens the
/// history as an operator instruction plus an acknowledgement, labelled for what it is rather than
/// dressed up as a platform prompt. Copied from `aq-translate.ts` (`SYSTEM_LEAD_IN`, `SYSTEM_ACK`).
const SYSTEM_LEAD_IN: &str =
    "Configuration for this session, set by the operator of this API gateway:";
const SYSTEM_ACK: &str =
    "Understood. I will follow that configuration for the rest of this conversation.";

/// Cursor validates tool names against `^[a-zA-Z0-9_-]+$` and caps them at 128 bytes; the same
/// limits are applied by `jcode-provider-cursor-runtime/src/wire.rs` (`MAX_CURSOR_TOOL_NAME_LEN`,
/// `sanitize_cursor_name`).
const MAX_TOOL_NAME_LEN: usize = 128;

/// The per-turn payload, owned so the request can outlive the borrow of `ctx`.
#[derive(Clone, Debug, Default)]
pub struct Prepared {
    /// One JSON document per history entry, in order; the caller stores them as blobs.
    pub blobs: Vec<Vec<u8>>,
    /// The text to send as this turn's user message. `None` means "resume"; it is only used for a
    /// history with nothing in it, because every other history is continued explicitly.
    pub action: Option<String>,
    pub tools: Vec<wire::McpTool>,
}

/// Builds the replay payload for one turn.
///
/// The whole history is replayed, with no truncation: the service rebuilds the prompt from these
/// entries, so dropping any would change what the model sees. A very long conversation is therefore
/// re-sent in full on every turn and can eventually be rejected by the service for size; the cap
/// belongs here, in the loop below — before an entry is pushed to `blobs`, keep the system exchange
/// and the newest entries whose bytes fit under the service's limit — never in `provider.rs`, which
/// only stores what this returns.
pub fn prepare(ctx: &Context) -> Prepared {
    let mut blobs = Vec::new();

    if let Some(system) = ctx
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|system| !system.is_empty())
    {
        push_entry(
            &mut blobs,
            json!({
                "role": "user",
                "content": [{ "type": "text", "text": format!("{SYSTEM_LEAD_IN}\n\n{system}") }],
            }),
        );
        push_entry(
            &mut blobs,
            json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": SYSTEM_ACK }],
            }),
        );
    }

    // A trailing user message is this turn's action, delivered as `UserMessageAction`; replaying it
    // as a history entry as well would send it twice.
    let action_index = match ctx.messages.last() {
        Some(Message::User(_)) => ctx.messages.len() - 1,
        _ => usize::MAX,
    };

    for (index, message) in ctx.messages.iter().enumerate() {
        if index == action_index {
            continue;
        }
        match message {
            Message::User(user) => {
                let text = user_text(&user.content);
                // An image-only turn has no text to replay.
                if !text.is_empty() {
                    push_entry(
                        &mut blobs,
                        json!({ "role": "user", "content": [{ "type": "text", "text": text }] }),
                    );
                }
            }
            Message::Assistant(assistant) => {
                let mut parts = Vec::new();
                for block in &assistant.content {
                    match block {
                        Content::Text(text) => {
                            parts.push(json!({ "type": "text", "text": text.text }))
                        }
                        Content::ToolCall(call) => parts.push(json!({
                            "type": "tool-call",
                            "toolCallId": call.id,
                            "toolName": call.name,
                            "args": call.arguments,
                        })),
                        // Reasoning is not replayed: the service produces its own.
                        Content::Thinking(_) | Content::Image(_) => {}
                    }
                }
                // A message that held only thinking (or nothing) has nothing to replay.
                if !parts.is_empty() {
                    push_entry(&mut blobs, json!({ "role": "assistant", "content": parts }));
                }
            }
            Message::ToolResult(result) => {
                // Emitted even when the result text is empty: the matching `tool-call` is already in
                // the history, and dropping the entry would replay an orphaned call.
                let mut entry = json!({
                    "role": "tool",
                    "id": result.tool_call_id,
                    "content": [{
                        "type": "tool-result",
                        "toolName": result.tool_name,
                        "toolCallId": result.tool_call_id,
                        "result": Content::text_only(&result.content, "\n"),
                    }],
                });
                if result.is_error {
                    if let Value::Object(map) = &mut entry {
                        map.insert("isError".to_string(), Value::Bool(true));
                    }
                }
                push_entry(&mut blobs, entry);
            }
        }
    }

    let action = match ctx.messages.last() {
        // Nothing happened yet: there is nothing to add.
        None => None,
        Some(Message::User(user)) => Some(user_text(&user.content)),
        // The history ends on a tool result or an assistant message, and a stateless replay has no
        // server-side turn to resume from.
        Some(_) => Some(CONTINUATION_PROMPT.to_string()),
    };

    let tools = ctx
        .tools
        .iter()
        .map(|tool| wire::McpTool {
            name: tool_name_for(&tool.name),
            tool_name: tool.name.clone(),
            provider_identifier: wire::TOOL_PROVIDER.to_string(),
            description: tool.description.clone(),
            input_schema: tool.parameters.clone().into_value(),
        })
        .collect();

    Prepared {
        blobs,
        action,
        tools,
    }
}

/// Maps a tool the model called (`zephyr_read`, provider `zephyr`) back to the host's tool name
/// (`read`). `None` when the id/name pair names no tool we advertised.
pub fn resolve_tool_name(
    tools: &[wire::McpTool],
    provider_identifier: &str,
    name: &str,
) -> Option<String> {
    if let Some(tool) = tools.iter().find(|tool| tool.name == name) {
        return Some(tool.tool_name.clone());
    }
    // A model that answers with the host's own name (the `tool_name` field) is still ours to serve,
    // but only when the call is addressed to this provider: another provider's names are not ours
    // to reinterpret.
    let ours = provider_identifier.is_empty() || provider_identifier == wire::TOOL_PROVIDER;
    if ours {
        if let Some(tool) = tools.iter().find(|tool| tool.tool_name == name) {
            return Some(tool.tool_name.clone());
        }
    }
    None
}

/// The wire name Cursor sees: our namespace plus the host name restricted to the charset Cursor
/// accepts, capped at [`MAX_TOOL_NAME_LEN`] bytes (the prefix and the sanitised body are ASCII, so
/// bytes and chars agree and truncation lands on a character boundary).
fn tool_name_for(host_name: &str) -> String {
    let mut name = String::with_capacity(wire::TOOL_PREFIX.len() + host_name.len());
    name.push_str(wire::TOOL_PREFIX);
    name.extend(host_name.chars().map(|ch| {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            ch
        } else {
            '_'
        }
    }));
    name.truncate(MAX_TOOL_NAME_LEN);
    name
}

/// The text of a user message: its string form, or the joined text blocks of its block form.
fn user_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => Content::text_only(blocks, "\n"),
    }
}

/// Appends one entry as compact JSON: the bytes are hashed into the blob id, so the encoding must be
/// stable, and `Value`'s `Display` is exactly `serde_json`'s compact form.
fn push_entry(blobs: &mut Vec<Vec<u8>>, entry: Value) {
    blobs.push(entry.to_string().into_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::{Api, AssistantMessage, Schema, Tool};

    fn entries(prepared: &Prepared) -> Vec<Value> {
        prepared
            .blobs
            .iter()
            .map(|blob| serde_json::from_slice(blob).expect("each blob is a JSON document"))
            .collect()
    }

    fn user(text: &str) -> Message {
        Message::User(rpi_ai::UserMessage::new(text, 0))
    }

    fn assistant(blocks: Vec<Content>) -> Message {
        let mut message = AssistantMessage::empty(Api::Faux, crate::ID, "model", 0);
        message.content = blocks;
        Message::Assistant(Box::new(message))
    }

    fn tool_result(id: &str, name: &str, text: &str, is_error: bool) -> Message {
        serde_json::from_value(json!({
            "role": "toolResult",
            "toolCallId": id,
            "toolName": name,
            "content": [{ "type": "text", "text": text }],
            "isError": is_error,
            "timestamp": 0,
        }))
        .expect("rpi's tool result shape")
    }

    fn host_tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: format!("the {name} tool"),
            parameters: Schema::new(json!({ "type": "object" })),
            constrained_sampling: None,
        }
    }

    #[test]
    fn the_system_prompt_becomes_an_operator_exchange() {
        let mut ctx = Context::new(vec![user("hi")]);
        ctx.system_prompt = Some("Be terse.".to_string());

        let prepared = prepare(&ctx);
        let entries = entries(&prepared);

        assert_eq!(
            entries[0],
            json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("{SYSTEM_LEAD_IN}\n\nBe terse."),
                }],
            })
        );
        assert_eq!(
            entries[1],
            json!({ "role": "assistant", "content": [{ "type": "text", "text": SYSTEM_ACK }] })
        );
        // No `system` role is ever emitted: the service drops it.
        assert!(entries.iter().all(|entry| entry["role"] != "system"));
    }

    #[test]
    fn a_blank_system_prompt_adds_no_exchange() {
        let mut ctx = Context::new(vec![user("hi"), user("there")]);
        ctx.system_prompt = Some("   ".to_string());

        let prepared = prepare(&ctx);

        assert_eq!(prepared.blobs.len(), 1);
        assert_eq!(entries(&prepared)[0]["role"], "user");
    }

    #[test]
    fn user_and_assistant_entries_keep_their_order_and_block_shapes() {
        let ctx = Context::new(vec![
            user("look at this"),
            assistant(vec![
                Content::thinking("hmm"),
                Content::text("reading"),
                Content::tool_call("call-1", "read", json!({ "path": "a.txt" })),
            ]),
            tool_result("call-1", "read", "file body", false),
            user("now edit it"),
        ]);

        let prepared = prepare(&ctx);
        let entries = entries(&prepared);

        assert_eq!(
            entries[0],
            json!({ "role": "user", "content": [{ "type": "text", "text": "look at this" }] })
        );
        assert_eq!(
            entries[1],
            json!({
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "reading" },
                    {
                        "type": "tool-call",
                        "toolCallId": "call-1",
                        "toolName": "read",
                        "args": { "path": "a.txt" },
                    },
                ],
            })
        );
        assert_eq!(
            entries[2],
            json!({
                "role": "tool",
                "id": "call-1",
                "content": [{
                    "type": "tool-result",
                    "toolName": "read",
                    "toolCallId": "call-1",
                    "result": "file body",
                }],
            })
        );
        // The trailing user message is the action, not an entry.
        assert_eq!(entries.len(), 3);
        assert_eq!(prepared.action.as_deref(), Some("now edit it"));
        // Blob bytes are compact: one line per entry, no pretty printing.
        assert!(prepared
            .blobs
            .iter()
            .all(|blob| !String::from_utf8_lossy(blob).contains('\n')));
    }

    #[test]
    fn tool_call_and_tool_result_ids_are_replayed_verbatim() {
        let ctx = Context::new(vec![
            assistant(vec![Content::tool_call(
                "call-abc_1",
                "bash",
                json!({ "command": "ls" }),
            )]),
            tool_result("call-abc_1", "bash", "a.txt", false),
        ]);

        let entries = entries(&prepare(&ctx));

        assert_eq!(entries[0]["content"][0]["toolCallId"], "call-abc_1");
        assert_eq!(entries[1]["content"][0]["toolCallId"], "call-abc_1");
        assert_eq!(entries[1]["id"], "call-abc_1");
    }

    #[test]
    fn an_error_result_is_marked_in_its_entry() {
        let ctx = Context::new(vec![tool_result("call-1", "read", "no such file", true)]);

        let entries = entries(&prepare(&ctx));

        assert_eq!(entries[0]["isError"], true);
        assert_eq!(entries[0]["content"][0]["result"], "no such file");
    }

    #[test]
    fn an_empty_tool_result_is_still_replayed() {
        let ctx = Context::new(vec![tool_result("call-1", "write", "", false)]);

        let entries = entries(&prepare(&ctx));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["content"][0]["result"], "");
        assert!(entries[0].get("isError").is_none());
    }

    #[test]
    fn a_history_ending_on_a_tool_result_asks_for_a_continuation() {
        let ctx = Context::new(vec![
            user("run it"),
            assistant(vec![Content::tool_call("call-1", "bash", json!({}))]),
            tool_result("call-1", "bash", "ok", false),
        ]);

        let prepared = prepare(&ctx);

        assert_eq!(prepared.action.as_deref(), Some(CONTINUATION_PROMPT));
        // The tool result is still replayed: the continuation prompt resumes from it.
        assert_eq!(entries(&prepared).len(), 3);
    }

    #[test]
    fn a_history_ending_on_an_assistant_message_asks_for_a_continuation() {
        let ctx = Context::new(vec![user("hi"), assistant(vec![Content::text("hello")])]);

        assert_eq!(prepare(&ctx).action.as_deref(), Some(CONTINUATION_PROMPT));
    }

    #[test]
    fn an_empty_history_resumes() {
        let prepared = prepare(&Context::default());

        assert!(prepared.blobs.is_empty());
        assert_eq!(prepared.action, None);
        assert!(prepared.tools.is_empty());
    }

    #[test]
    fn tools_are_namespaced_for_the_wire_and_keep_the_host_name() {
        let ctx = Context {
            tools: vec![host_tool("read"), host_tool("web.search/one")],
            ..Context::default()
        };

        let tools = prepare(&ctx).tools;

        assert_eq!(tools[0].name, "zephyr_read");
        assert_eq!(tools[0].tool_name, "read");
        assert_eq!(tools[0].provider_identifier, wire::TOOL_PROVIDER);
        assert_eq!(tools[0].description, "the read tool");
        assert_eq!(tools[0].input_schema, json!({ "type": "object" }));
        assert_eq!(tools[1].name, "zephyr_web_search_one");
        assert_eq!(tools[1].tool_name, "web.search/one");
    }

    #[test]
    fn long_tool_names_are_capped_at_the_wire_limit() {
        let ctx = Context {
            tools: vec![host_tool(&"n".repeat(300))],
            ..Context::default()
        };

        let name = &prepare(&ctx).tools[0].name;

        assert_eq!(name.len(), MAX_TOOL_NAME_LEN);
        assert!(name.starts_with("zephyr_nnn"));
        assert!(name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'));
    }

    #[test]
    fn resolve_tool_name_round_trips_a_call_and_rejects_unknown_tools() {
        let ctx = Context {
            tools: vec![host_tool("read"), host_tool("bash")],
            ..Context::default()
        };
        let tools = prepare(&ctx).tools;

        assert_eq!(
            resolve_tool_name(&tools, wire::TOOL_PROVIDER, "zephyr_read").as_deref(),
            Some("read")
        );
        assert_eq!(
            resolve_tool_name(&tools, "", "bash").as_deref(),
            Some("bash")
        );
        assert_eq!(resolve_tool_name(&tools, "zephyr", "nope"), None);
        // Another provider's bare name is not ours to reinterpret.
        assert_eq!(resolve_tool_name(&tools, "openai", "read"), None);
        assert_eq!(
            resolve_tool_name(&[], wire::TOOL_PROVIDER, "zephyr_read"),
            None
        );
    }
}
