//! End-to-end tests for the Connect transport: the real [`Session`] against a fake agent service on
//! `127.0.0.1`, so framing, gunzip, heartbeats, KV replies and trailers are exercised over an
//! actual HTTP/2 connection without touching the network.
//!
//! The fake encodes and decodes protobuf by hand (see `tests/support/fake_agent.rs`), independently
//! of `src/wire.rs`, so these tests check the wire format rather than mirroring it.
#[path = "support/fake_agent.rs"]
mod fake_agent;

use fake_agent::{ExpectHeaders, FakeAgent, Step};
use std::time::Duration;
use tokio::time::timeout;

// Integration tests reach the real transport through the crate's public modules (the cdylib also
// builds an rlib for exactly this).
use zephyr_cursor::transport::{BlobStore, Session, SessionConfig};
use zephyr_cursor::wire::{self, ServerBody};

/// Every wait is bounded: a transport that hangs must fail a test, not stall the suite.
const DEADLINE: Duration = Duration::from_secs(10);
const TOKEN: &str = "test-token-9f2";
const CONVERSATION_ID: &str = "conversation-1";
const REQUEST_ID: &str = "request-1";

fn config(url: &str) -> SessionConfig {
    SessionConfig {
        url: url.to_string(),
        token: TOKEN.to_string(),
        conversation_id: CONVERSATION_ID.to_string(),
        request_id: REQUEST_ID.to_string(),
    }
}

async fn connect(url: &str) -> Session {
    // Production heartbeats every 5 s; tests need silence to be observable well inside the
    // deadline, and `CURSOR_HEARTBEAT_MS` is the documented override for exactly that.
    std::env::set_var("CURSOR_HEARTBEAT_MS", "200");
    timeout(DEADLINE, Session::connect(&config(url), run_request()))
        .await
        .expect("connect within the deadline")
        .expect("connect to the fake agent")
}

/// A minimal `AgentRunRequest`; the fake only requires that it arrives before anything else.
fn run_request() -> Vec<u8> {
    wire::encode_run_request(&wire::RunRequest {
        blobs: &[],
        model: "test-model",
        conversation_id: CONVERSATION_ID,
        tools: &[],
        action: wire::Action::UserText {
            text: "hello".to_string(),
            message_id: "message-1".to_string(),
        },
    })
}

async fn next(session: &mut Session) -> Option<wire::ServerMsg> {
    timeout(DEADLINE, session.next())
        .await
        .expect("a server message within the deadline")
        .expect("decode a server message")
}

#[tokio::test]
async fn connect_sends_the_connect_headers_and_heartbeats_on_silence() {
    let agent = FakeAgent::start(
        ExpectHeaders {
            token: Some(TOKEN.to_string()),
            request_id: Some(REQUEST_ID.to_string()),
            conversation_id: Some(CONVERSATION_ID.to_string()),
            ..ExpectHeaders::default()
        },
        vec![
            Step::ExpectRunRequest,
            // Several heartbeat intervals of server silence: the client must speak first, and the
            // fake fails the test if it stays mute.
            Step::ExpectHeartbeat {
                silence: Duration::from_millis(1200),
            },
            Step::clean_end(),
        ],
    )
    .await;

    let mut session = connect(&agent.url).await;
    assert!(
        next(&mut session).await.is_none(),
        "an empty trailer ends the stream"
    );
    agent.finish().await;
}

#[tokio::test]
async fn text_deltas_arrive_in_order_after_a_gzip_frame() {
    let agent = FakeAgent::start(
        ExpectHeaders::default(),
        vec![
            Step::ExpectRunRequest,
            Step::text("Hello"),
            // The second frame is gzip-flagged: the transport must decompress it in place.
            Step::gzipped_text(", world"),
            Step::ThinkingDelta {
                text: "considering".to_string(),
            },
            Step::TokenDelta { tokens: 7 },
            Step::TurnEnded,
            Step::clean_end(),
        ],
    )
    .await;

    let mut session = connect(&agent.url).await;
    let mut text = Vec::new();
    let mut thinking = Vec::new();
    let mut tokens = Vec::new();
    let mut ended = false;
    while let Some(message) = next(&mut session).await {
        match message.body {
            ServerBody::TextDelta(delta) => text.push(delta),
            ServerBody::ThinkingDelta(delta) => thinking.push(delta),
            ServerBody::Tokens(count) => tokens.push(count),
            ServerBody::TurnEnded { .. } => ended = true,
            other => panic!("unexpected server message {other:?}"),
        }
    }
    assert_eq!(
        text,
        ["Hello", ", world"],
        "text deltas must arrive in order, with the gzip frame decompressed"
    );
    assert_eq!(thinking, ["considering"]);
    assert_eq!(tokens, [7]);
    assert!(ended, "the turn must end with turn_ended");
    agent.finish().await;
}

#[tokio::test]
async fn clean_trailer_ends_the_stream_after_the_buffered_frames() {
    let agent = FakeAgent::start(
        ExpectHeaders::default(),
        vec![
            Step::ExpectRunRequest,
            Step::text("only message"),
            Step::clean_end(),
        ],
    )
    .await;

    let mut session = connect(&agent.url).await;
    let message = next(&mut session).await.expect("a text delta");
    assert_eq!(
        message.body,
        ServerBody::TextDelta("only message".to_string())
    );
    assert!(
        next(&mut session).await.is_none(),
        "a {{}} trailer ends the stream once the frames before it are consumed"
    );
    assert!(
        next(&mut session).await.is_none(),
        "the end of the stream stays ended"
    );
    agent.finish().await;
}

#[tokio::test]
async fn request_context_is_answered() {
    let agent = FakeAgent::start(
        ExpectHeaders::default(),
        vec![
            Step::ExpectRunRequest,
            Step::request_context(4, "exec-4"),
            Step::clean_end(),
        ],
    )
    .await;

    let mut session = connect(&agent.url).await;
    let message = next(&mut session).await.expect("a request_context");
    assert_eq!(message.body, ServerBody::RequestContext);
    assert_eq!((message.id, message.exec_id.as_str()), (4, "exec-4"));
    session
        .send(wire::encode_request_context_result(
            message.id,
            &message.exec_id,
            &[],
        ))
        .await
        .expect("answer the handshake");
    assert!(
        next(&mut session).await.is_none(),
        "the stream ends once the handshake is answered"
    );
    agent.finish().await;
}

#[tokio::test]
async fn blob_round_trip_answers_with_the_stored_bytes() {
    let blobs = BlobStore::new();
    let entry = br#"{"role":"user","text":"hi"}"#.to_vec();
    let blob_id = blobs.insert(entry.clone());
    let agent = FakeAgent::start(
        ExpectHeaders::default(),
        vec![
            Step::ExpectRunRequest,
            Step::KvGetBlob {
                blob_id: blob_id.clone(),
                expected: entry.clone(),
            },
            Step::clean_end(),
        ],
    )
    .await;

    let mut session = connect(&agent.url).await;
    let message = next(&mut session).await.expect("a blob request");
    match &message.body {
        ServerBody::KvGetBlob { blob_id } => {
            let data = blobs.get(blob_id).unwrap_or_default();
            session
                .send(wire::encode_kv_get_blob_result(message.id, &data))
                .await
                .expect("answer the blob request");
        }
        other => panic!("expected a blob request, got {other:?}"),
    }
    assert!(next(&mut session).await.is_none());
    agent.finish().await;
}

#[tokio::test]
async fn mcp_args_ends_the_turn_without_a_reply() {
    let agent = FakeAgent::start(
        ExpectHeaders::default(),
        vec![
            Step::ExpectRunRequest,
            Step::McpArgs {
                call_id: "call-1".to_string(),
                tool_name: "zephyr_read".to_string(),
                args: vec![("path".to_string(), "src/lib.rs".to_string())],
            },
            Step::clean_end(),
        ],
    )
    .await;

    let mut session = connect(&agent.url).await;
    let message = next(&mut session).await.expect("a tool call");
    match message.body {
        ServerBody::McpArgs {
            call_id,
            tool_name,
            args,
            ..
        } => {
            assert_eq!(call_id, "call-1");
            assert!(tool_name.ends_with("read"), "tool name {tool_name:?}");
            assert_eq!(args["path"], "src/lib.rs");
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
    // The provider ends the turn here and lets the host run the tool; the fake fails the test if
    // the client answers with anything but a heartbeat.
    assert!(next(&mut session).await.is_none());
    session.close().await;
    agent.finish().await;
}

#[tokio::test]
async fn error_trailer_fails_the_turn_with_its_message() {
    let agent = FakeAgent::start(
        ExpectHeaders::default(),
        vec![
            Step::ExpectRunRequest,
            Step::text("partial"),
            Step::error_end("resource_exhausted", "quota exceeded for this plan"),
        ],
    )
    .await;

    let mut session = connect(&agent.url).await;
    let first = next(&mut session).await.expect("a text delta");
    assert_eq!(first.body, ServerBody::TextDelta("partial".to_string()));
    let error = match timeout(DEADLINE, session.next()).await {
        Ok(Err(error)) => error,
        Ok(Ok(message)) => panic!("an error trailer must fail the stream, got {message:?}"),
        Err(_) => panic!("the failure must arrive within the deadline"),
    };
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("quota exceeded for this plan"),
        "the error must carry the server's message, got {rendered}"
    );
    assert!(
        rendered.contains("resource_exhausted"),
        "the error must carry the server's code, got {rendered}"
    );
    agent.finish().await;
}

#[tokio::test]
async fn non_200_status_fails_with_a_clear_error() {
    let agent = FakeAgent::start_error(503, "agent service is down for maintenance").await;
    let result = timeout(
        DEADLINE,
        Session::connect(&config(&agent.url), run_request()),
    )
    .await
    .expect("connect fails within the deadline");
    let error = match result {
        Ok(_) => panic!("a 503 must fail the connection"),
        Err(error) => error,
    };
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("503"),
        "the error must name the status, got {rendered}"
    );
    assert!(
        rendered.contains("agent service is down for maintenance"),
        "the error must include the response body, got {rendered}"
    );
    agent.finish().await;
}
