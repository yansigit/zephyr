//! End to end against the pinned real `rpi`. The model is a local OpenAI-compatible
//! stub declared in a temp agent dir's models.json, so no real provider is called.
//! Run with: cargo test -p zephyr --test real_rpi -- --ignored

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Channel, Inbound, Outbound};
use zephyr::config::Config;

const RPI: &str = "/home/user/Projects/.always-on/rpi-0.14.0/bin/rpi";

struct TestChannel {
    feed: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    sent: Mutex<Vec<Outbound>>,
}

impl Channel for TestChannel {
    fn name(&self) -> &str {
        "chat"
    }
    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        while let Some(m) = self.feed.lock().await.recv().await {
            tx.send(m).await?;
        }
        std::future::pending().await
    }
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(msg.clone());
        Ok(())
    }
}

/// Streams "pong" for every chat completion; holds the request open while `hang` is set.
async fn stub_provider(hang: Arc<AtomicBool>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let hang = hang.swap(false, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0; 1 << 20];
                let mut got = Vec::new();
                while let Ok(n @ 1..) = sock.read(&mut buf).await {
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got);
                    let Some(end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if got.len() >= end + 4 + len {
                        break;
                    }
                }
                if hang {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                let chunk = |delta: &str, finish: &str| {
                    format!("data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n")
                };
                let body = chunk(r#"{"role":"assistant","content":"pong"}"#, "null")
                    + &chunk("{}", "\"stop\"")
                    + "data: [DONE]\n\n";
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test]
#[ignore = "needs the pinned rpi binary"]
async fn real_rpi_round_trip_and_kill_mid_run() {
    if !std::path::Path::new(RPI).exists() {
        eprintln!("skipping: {RPI} is not installed");
        return;
    }
    let hang = Arc::new(AtomicBool::new(false));
    let port = stub_provider(hang.clone()).await;
    let home = std::env::temp_dir().join(format!(
        "zephyr-real-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let agent = home.join("rpi-agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        format!(r#"{{"providers":{{"stub":{{"api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","apiKey":"not-a-key","models":[{{"id":"m","name":"M"}}]}}}}}}"#),
    )
    .unwrap();
    let cfg: Config = toml::from_str(&format!(
        "rpi = {RPI:?}\nhome = {home:?}\nagent_dir = {agent:?}\nmodel = \"stub/m\"\n[owners]\nchat = [\"owner\"]\n"
    ))
    .unwrap();

    let (feed, rx) = mpsc::channel(4);
    let channel = Arc::new(TestChannel {
        feed: tokio::sync::Mutex::new(rx),
        sent: Mutex::default(),
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let ch = channel.clone();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, std::slice::from_ref(&*ch), async {
            let _ = stopped.await;
        })
        .await
    });
    let say = |id: &str, text: &str| Inbound {
        channel: "chat".into(),
        platform_msg_id: id.into(),
        sender: "owner".into(),
        text: text.into(),
    };
    let replies = |n: usize| {
        let channel = channel.clone();
        async move {
            for _ in 0..400 {
                if channel.sent.lock().unwrap().len() >= n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            channel
                .sent
                .lock()
                .unwrap()
                .iter()
                .map(|m| m.text.clone())
                .collect::<Vec<_>>()
        }
    };

    feed.send(say("m1", "ping")).await.unwrap();
    assert_eq!(replies(1).await, ["pong"]);

    // Kill the child while the provider holds the second request open.
    hang.store(true, Ordering::SeqCst);
    feed.send(say("m2", "again")).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pattern = format!("--session-dir {}", home.join("sessions").display());
    let killed = std::process::Command::new("pkill")
        .args(["-9", "-f", "--", &pattern])
        .status()
        .unwrap();
    assert!(killed.success(), "no rpi child found");
    assert_eq!(replies(2).await, ["pong", "pong"]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        channel.sent.lock().unwrap().len(),
        2,
        "a reply was sent twice"
    );

    let _ = stop.send(());
    task.await.unwrap().unwrap();
}

/// Answers every request with HTTP 429 and counts them.
async fn rate_limited_provider(hits: Arc<std::sync::atomic::AtomicUsize>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            hits.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0; 1 << 16];
                let _ = sock.read(&mut buf).await;
                let body =
                    r#"{"error":{"message":"Rate limit reached","type":"rate_limit_error"}}"#;
                let resp = format!("HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test]
#[ignore = "needs the pinned rpi binary"]
async fn real_rpi_fails_over_to_the_next_account() {
    if !std::path::Path::new(RPI).exists() {
        eprintln!("skipping: {RPI} is not installed");
        return;
    }
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let limited = rate_limited_provider(hits.clone()).await;
    let ok = stub_provider(Arc::new(AtomicBool::new(false))).await;
    let home = std::env::temp_dir().join(format!(
        "zephyr-real-pool-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let agent = home.join("rpi-agent");
    std::fs::create_dir_all(&agent).unwrap();
    let provider = |port: u16| {
        format!(
            r#"{{"api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","apiKey":"not-a-key","models":[{{"id":"m","name":"M"}}]}}"#
        )
    };
    std::fs::write(
        agent.join("models.json"),
        format!(
            r#"{{"providers":{{"pa":{},"pb":{}}}}}"#,
            provider(limited),
            provider(ok)
        ),
    )
    .unwrap();
    let cfg: Config = toml::from_str(&format!(
        "rpi = {RPI:?}\nhome = {home:?}\nagent_dir = {agent:?}\n[owners]\nchat = [\"owner\"]\n[[account]]\nname = \"a\"\nmodel = \"pa/m\"\n[[account]]\nname = \"b\"\nmodel = \"pb/m\"\n"
    ))
    .unwrap();

    let (feed, rx) = mpsc::channel(4);
    let channel = Arc::new(TestChannel {
        feed: tokio::sync::Mutex::new(rx),
        sent: Mutex::default(),
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let ch = channel.clone();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, std::slice::from_ref(&*ch), async {
            let _ = stopped.await;
        })
        .await
    });
    feed.send(Inbound {
        channel: "chat".into(),
        platform_msg_id: "m1".into(),
        sender: "owner".into(),
        text: "ping".into(),
    })
    .await
    .unwrap();
    for _ in 0..600 {
        if channel.sent.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sent: Vec<String> = channel
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.text.clone())
        .collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent[0].starts_with("[zephyr] account a is rate-limited until "),
        "{sent:?}"
    );
    assert_eq!(sent[1], "pong");
    // The shared settings.json caps rpi's own retries at one.
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let _ = stop.send(());
    task.await.unwrap().unwrap();
}
