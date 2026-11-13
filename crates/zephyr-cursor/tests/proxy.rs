//! The stream through an HTTP `CONNECT` proxy, as the agent's processes reach the network under
//! the floor (DESIGN §4.7.2, §4.20). Its own test binary: the proxy variables are process-wide.
#[path = "support/fake_agent.rs"]
mod fake_agent;

use fake_agent::{ExpectHeaders, FakeAgent, Step};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use zephyr_cursor::transport::{Session, SessionConfig};
use zephyr_cursor::wire::{self, ServerBody};

const DEADLINE: Duration = Duration::from_secs(10);

/// A one-connection `CONNECT` proxy on loopback: records the request head it was sent, answers
/// 200 and joins the two sockets.
async fn proxy() -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(String::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            if client.read(&mut byte).await.unwrap() == 0 {
                return;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        *record.lock().unwrap() = head.clone();
        let target = head.split_whitespace().nth(1).unwrap().to_string();
        let mut upstream = TcpStream::connect(target).await.unwrap();
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .unwrap();
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    });
    (url, seen)
}

#[tokio::test]
async fn the_stream_goes_through_the_connect_proxy_the_environment_names() {
    let agent = FakeAgent::start(
        ExpectHeaders::default(),
        vec![
            Step::ExpectRunRequest,
            Step::text("through the proxy"),
            Step::clean_end(),
        ],
    )
    .await;
    let (proxy_url, seen) = proxy().await;
    std::env::set_var("HTTP_PROXY", &proxy_url);
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");

    let config = SessionConfig {
        url: agent.url.clone(),
        token: "t".into(),
        conversation_id: "c".into(),
        request_id: "r".into(),
    };
    let request = wire::encode_run_request(&wire::RunRequest {
        blobs: &[],
        model: "test-model",
        conversation_id: "c",
        tools: &[],
        action: wire::Action::UserText {
            text: "hello".into(),
            message_id: "m".into(),
        },
    });
    let mut session = timeout(DEADLINE, Session::connect(&config, request))
        .await
        .expect("connect within the deadline")
        .expect("connect through the proxy");
    let message = timeout(DEADLINE, session.next())
        .await
        .unwrap()
        .unwrap()
        .expect("a text delta");
    assert_eq!(
        message.body,
        ServerBody::TextDelta("through the proxy".into())
    );
    session.close().await;
    let url = agent.url.clone();
    agent.finish().await;

    let authority = url.trim_start_matches("http://");
    let authority = &authority[..authority.find('/').unwrap_or(authority.len())];
    let head = seen.lock().unwrap().clone();
    assert!(
        head.starts_with(&format!("CONNECT {authority} HTTP/1.1\r\n")),
        "{head:?}"
    );
    assert!(
        head.contains(&format!("\r\nHost: {authority}\r\n")),
        "{head:?}"
    );
}
