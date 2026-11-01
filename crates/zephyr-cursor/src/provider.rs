//! The rpi-side state machine: `stream_simple` opens one agent turn, translates its events, and
//! ends it on the first tool call.
//!
//! Content layout follows rpi's convention: thinking first, then text, then tool calls, each block
//! wrapped in `Start`/`Delta`*/`End` with a matching entry in every `partial`. The terminal `Done`
//! carries the full message, and a stop reason of `ToolUse` is what makes rpi execute the calls and
//! call us again with their results.
use crate::history::{self, Prepared};
use crate::{auth, models, transport, wire};
use rpi_ai::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStreamProducer,
};
use rpi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context,
    DoneReason, ErrorReason, Model, Provider, SimpleStreamOptions, StopReason, TextContent,
    ThinkingContent, ToolCall,
};
use std::sync::Arc;

/// The registered provider. Stateless: one turn's state lives in [`Turn`].
pub struct Cursor;

#[async_trait::async_trait]
impl Provider for Cursor {
    fn id(&self) -> &str {
        crate::ID
    }

    fn models(&self) -> &[Model] {
        models::catalog()
    }

    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut tx, rx) = create_assistant_message_event_stream();
        // Everything the turn needs is copied out here: the bridge's request borrows `model`,
        // `context` and `options` for the duration of this call only.
        let model = model.clone();
        let prepared = history::prepare(context);
        let key = options.api_key.clone();
        let session_id = options.session_id.clone();
        tokio::spawn(async move {
            let outcome = Turn::new(model.clone(), prepared, session_id)
                .run(&mut tx, key.as_deref())
                .await;
            if let Err(err) = outcome {
                let message = Turn::error_message(&model, &format!("{err:#}"));
                let _ = tx
                    .push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error: message,
                    })
                    .await;
            }
        });
        rx
    }
}

struct Turn {
    model: Model,
    prepared: Prepared,
    session_id: Option<String>,
}

impl Turn {
    fn new(model: Model, prepared: Prepared, session_id: Option<String>) -> Self {
        Self {
            model,
            prepared,
            session_id,
        }
    }

    /// One request/response exchange. Returns after the terminal event has been pushed.
    async fn run(
        &self,
        tx: &mut AssistantMessageEventStreamProducer,
        key: Option<&str>,
    ) -> anyhow::Result<()> {
        let token = auth::token(key).await?;
        let conversation_id = conversation_id(self.session_id.as_deref());
        let config = transport::SessionConfig {
            url: wire::endpoint(),
            token,
            conversation_id: conversation_id.clone(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };
        let blobs = transport::BlobStore::new();
        let ids: Vec<Vec<u8>> = self
            .prepared
            .blobs
            .iter()
            .cloned()
            .map(|entry| blobs.insert(entry))
            .collect();
        let action = match &self.prepared.action {
            Some(text) => wire::Action::UserText {
                text: text.clone(),
                message_id: uuid::Uuid::new_v4().to_string(),
            },
            None => wire::Action::Resume,
        };

        let run_request = wire::encode_run_request(&wire::RunRequest {
            blobs: &ids,
            model: &self.model.id,
            conversation_id: &conversation_id,
            tools: &self.prepared.tools,
            action,
        });
        let mut session = transport::Session::connect(&config, run_request).await?;

        let mut out = Output::new(self.model.clone());
        while let Some(message) = session.next().await? {
            match message.body {
                wire::ServerBody::TextDelta(delta) => out.text(tx, &delta).await,
                wire::ServerBody::ThinkingDelta(delta) => out.thinking(tx, &delta).await,
                wire::ServerBody::Tokens(tokens) => out.tokens(tokens),
                wire::ServerBody::TurnEnded {
                    input,
                    output,
                    cache_read,
                } => {
                    out.usage(input, output, cache_read);
                    break;
                }
                wire::ServerBody::Heartbeat | wire::ServerBody::Ignored => {}
                wire::ServerBody::KvGetBlob { blob_id } => {
                    let data = blobs.get(&blob_id).unwrap_or_default();
                    session
                        .send(wire::encode_kv_get_blob_result(message.id, &data))
                        .await?;
                }
                wire::ServerBody::KvSetBlob { blob_id, data } => {
                    blobs.put(blob_id, data);
                    session
                        .send(wire::encode_kv_set_blob_ack(message.id))
                        .await?;
                }
                wire::ServerBody::RequestContext => {
                    let reply = wire::encode_request_context_result(
                        message.id,
                        &message.exec_id,
                        &self.prepared.tools,
                    );
                    session.send(reply).await?;
                }
                wire::ServerBody::McpState => {
                    let reply = wire::encode_mcp_state_result(
                        message.id,
                        &message.exec_id,
                        &self.prepared.tools,
                    );
                    session.send(reply).await?;
                }
                wire::ServerBody::UnknownExec { member } => {
                    session
                        .send(wire::encode_exec_throw(message.id, member))
                        .await?;
                }
                wire::ServerBody::RejectExec { kind } => {
                    let reply = wire::encode_exec_rejection(message.id, &message.exec_id, &kind);
                    session.send(reply).await?;
                }
                wire::ServerBody::McpArgs {
                    call_id,
                    provider_identifier,
                    tool_name,
                    args,
                } => {
                    // The turn ends here on purpose: rpi runs the tool, appends the result, and
                    // asks again with the whole history. Keeping the stream open to send the result
                    // back ourselves would duplicate a loop rpi already owns.
                    out.push_call(
                        call_id,
                        provider_identifier,
                        tool_name,
                        args,
                        &self.prepared,
                    );
                    break;
                }
            }
        }
        out.finish(tx).await;
        session.close().await;
        Ok(())
    }

    /// The terminal `Error` message, shaped like every other provider's.
    fn error_message(model: &Model, error: &str) -> AssistantMessage {
        let mut message = AssistantMessage::empty(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            now(),
        );
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error.to_string());
        message
    }
}

/// A stable conversation id per rpi session: Cursor's server keeps per-conversation state, and a
/// fresh id every turn would pay for the same prompt again each time.
fn conversation_id(session_id: Option<&str>) -> String {
    match session_id {
        Some(id) if !id.is_empty() => {
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, id.as_bytes()).to_string()
        }
        _ => uuid::Uuid::new_v4().to_string(),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Emits the event stream for one turn while keeping the `partial` message consistent with it.
struct Output {
    model: Model,
    content: Vec<Content>,
    thinking_index: Option<usize>,
    text_index: Option<usize>,
    tokens: i64,
    prompt: i64,
    cache_read: i64,
    calls: Vec<ToolCall>,
}

impl Output {
    fn new(model: Model) -> Self {
        Self {
            model,
            content: Vec::new(),
            thinking_index: None,
            text_index: None,
            tokens: 0,
            prompt: 0,
            cache_read: 0,
            calls: Vec::new(),
        }
    }

    fn message(&self, stop_reason: StopReason) -> AssistantMessage {
        let mut message = AssistantMessage::empty(
            self.model.api.clone(),
            self.model.provider.clone(),
            self.model.id.clone(),
            now(),
        );
        message.content = self.content.clone();
        message.stop_reason = stop_reason;
        message.usage.input = self.prompt;
        message.usage.output = self.tokens;
        message.usage.cache_read = self.cache_read;
        message.usage.total_tokens = self.prompt + self.tokens;
        message
    }

    async fn thinking(&mut self, tx: &mut AssistantMessageEventStreamProducer, delta: &str) {
        if self.thinking_index.is_none() {
            self.content.push(Content::Thinking(ThinkingContent {
                kind: Default::default(),
                thinking: String::new(),
                thinking_signature: None,
                redacted: false,
            }));
            let index = self.content.len() - 1;
            self.thinking_index = Some(index);
            let partial = Arc::new(self.message(StopReason::Stop));
            tx.push(AssistantMessageEvent::ThinkingStart {
                content_index: index,
                partial,
            })
            .await;
        }
        let index = self.thinking_index.unwrap_or_default();
        if let Some(Content::Thinking(block)) = self.content.get_mut(index) {
            block.thinking.push_str(delta);
        }
        let partial = Arc::new(self.message(StopReason::Stop));
        tx.push(AssistantMessageEvent::ThinkingDelta {
            content_index: index,
            delta: delta.to_string(),
            partial,
        })
        .await;
    }

    async fn text(&mut self, tx: &mut AssistantMessageEventStreamProducer, delta: &str) {
        if self.text_index.is_none() {
            self.content.push(Content::Text(TextContent {
                kind: Default::default(),
                text: String::new(),
                text_signature: None,
            }));
            let index = self.content.len() - 1;
            self.text_index = Some(index);
            let partial = Arc::new(self.message(StopReason::Stop));
            tx.push(AssistantMessageEvent::TextStart {
                content_index: index,
                partial,
            })
            .await;
        }
        let index = self.text_index.unwrap_or_default();
        if let Some(Content::Text(block)) = self.content.get_mut(index) {
            block.text.push_str(delta);
        }
        let partial = Arc::new(self.message(StopReason::Stop));
        tx.push(AssistantMessageEvent::TextDelta {
            content_index: index,
            delta: delta.to_string(),
            partial,
        })
        .await;
    }

    /// The engine reports a running total, not a delta.
    fn tokens(&mut self, tokens: i64) {
        self.tokens = tokens.max(0);
    }

    /// The closing frame's totals: authoritative for the turn, so they replace the running count
    /// (`None` for a field the engine omits keeps what the stream already said).
    fn usage(&mut self, input: Option<i64>, output: Option<i64>, cache_read: Option<i64>) {
        if let Some(input) = input {
            self.prompt = input.max(0);
        }
        if let Some(output) = output {
            self.tokens = output.max(0);
        }
        if let Some(cache_read) = cache_read {
            self.cache_read = cache_read.max(0);
        }
    }

    fn push_call(
        &mut self,
        call_id: String,
        provider_identifier: String,
        tool_name: String,
        args: serde_json::Value,
        prepared: &Prepared,
    ) {
        let name = history::resolve_tool_name(&prepared.tools, &provider_identifier, &tool_name)
            .unwrap_or(tool_name);
        self.calls.push(ToolCall {
            kind: Default::default(),
            id: wire::safe_call_id(&call_id),
            name,
            arguments: args,
            thought_signature: None,
            namespace: None,
            recorded: None,
        });
    }

    /// Closes the open blocks, emits the calls, and pushes the terminal event.
    async fn finish(self, tx: &mut AssistantMessageEventStreamProducer) {
        let mut this = self;
        if let Some(index) = this.thinking_index {
            let content = match this.content.get(index) {
                Some(Content::Thinking(block)) => block.thinking.clone(),
                _ => String::new(),
            };
            let partial = Arc::new(this.message(StopReason::Stop));
            tx.push(AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content,
                partial,
            })
            .await;
        }
        if let Some(index) = this.text_index {
            let content = match this.content.get(index) {
                Some(Content::Text(block)) => block.text.clone(),
                _ => String::new(),
            };
            let partial = Arc::new(this.message(StopReason::Stop));
            tx.push(AssistantMessageEvent::TextEnd {
                content_index: index,
                content,
                partial,
            })
            .await;
        }
        for call in this.calls.clone() {
            this.content.push(Content::ToolCall(call.clone()));
            let index = this.content.len() - 1;
            let partial = Arc::new(this.message(StopReason::Stop));
            tx.push(AssistantMessageEvent::ToolCallStart {
                content_index: index,
                partial,
            })
            .await;
            let partial = Arc::new(this.message(StopReason::ToolUse));
            tx.push(AssistantMessageEvent::ToolCallEnd {
                content_index: index,
                tool_call: call,
                partial,
            })
            .await;
        }
        let tool_use = !this.calls.is_empty();
        let stop_reason = if tool_use {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        let done = if tool_use {
            DoneReason::ToolUse
        } else {
            DoneReason::Stop
        };
        let message = this.message(stop_reason);
        tx.push(AssistantMessageEvent::Done {
            reason: done,
            message,
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_maps_to_one_stable_conversation_and_no_session_to_a_fresh_one() {
        assert_eq!(conversation_id(Some("s-1")), conversation_id(Some("s-1")));
        assert_ne!(conversation_id(Some("s-1")), conversation_id(Some("s-2")));
        assert_ne!(conversation_id(None), conversation_id(None));
    }
}
